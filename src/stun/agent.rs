// Copyright (C) 2020 Matthew Waters <matthew@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use std::collections::HashMap;

use futures::future::AbortHandle;
use futures::future::Either;
use futures::prelude::*;
use futures_timer::Delay;

use crate::agent::AgentError;

use crate::stun::attribute::*;
use crate::stun::message::*;

use crate::socket::ChannelBroadcast;

use crate::socket::UdpSocketChannel;

#[derive(Debug)]
pub struct StunAgent {
    state: Arc<Mutex<StunAgentState>>,
    pub(crate) channel: Arc<UdpSocketChannel>,
    stun_broadcaster: Arc<ChannelBroadcast<(Message, SocketAddr)>>,
    data_broadcaster: Arc<ChannelBroadcast<(Vec<u8>, SocketAddr)>>,
    stun_send_send: async_channel::Sender<(Message, SocketAddr)>,
}

#[derive(Debug)]
struct StunAgentState {
    receive_loop_started: bool,
    outstanding_requests: HashMap<u128, Message>,
    local_credentials: Option<MessageIntegrityCredentials>,
    remote_credentials: Option<MessageIntegrityCredentials>,
}

impl StunAgent {
    pub fn new(channel: Arc<UdpSocketChannel>) -> Self {
        let (stun_send_send, mut stun_send_recv) =
            async_channel::bounded::<(Message, SocketAddr)>(16);
        let state = Arc::new(Mutex::new(StunAgentState::new()));

        async_std::task::spawn({
            let channel = channel.clone();

            async move {
                while let Some((msg, to)) = stun_send_recv.next().await {
                    trace!("StunAgent channel send message {}", msg);
                    StunAgent::maybe_store_message(state.clone(), msg.clone());
                    let buf = msg.to_bytes();
                    let _ = channel.send_to(&buf, to).await;
                }
                debug!("StunAgent channel send loop exited");
            }
        });

        Self {
            state: Arc::new(Mutex::new(StunAgentState::new())),
            channel,
            stun_broadcaster: Arc::new(ChannelBroadcast::default()),
            data_broadcaster: Arc::new(ChannelBroadcast::default()),
            stun_send_send,
        }
    }

    fn maybe_store_message(state: Arc<Mutex<StunAgentState>>, msg: Message) {
        if msg.has_class(MessageClass::Request) {
            let mut state = state.lock().unwrap();
            state
                .outstanding_requests
                .insert(msg.transaction_id(), msg.clone());
        }
    }

    pub fn set_local_credentials(&self, credentials: MessageIntegrityCredentials) {
        let mut state = self.state.lock().unwrap();
        state.local_credentials = Some(credentials)
    }

    pub fn local_credentials(&self) -> Option<MessageIntegrityCredentials> {
        let state = self.state.lock().unwrap();
        state.local_credentials.clone()
    }

    pub fn set_remote_credentials(&self, credentials: MessageIntegrityCredentials) {
        let mut state = self.state.lock().unwrap();
        state.remote_credentials = Some(credentials)
    }

    pub fn remote_credentials(&self) -> Option<MessageIntegrityCredentials> {
        let state = self.state.lock().unwrap();
        state.remote_credentials.clone()
    }

    pub async fn send(&self, msg: Message, to: SocketAddr) -> Result<(), std::io::Error> {
        StunAgent::maybe_store_message(self.state.clone(), msg.clone());
        self.stun_send_send.send((msg, to)).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "Channel closed")
        })
    }

    fn receive_task_loop(
        state: Arc<Mutex<StunAgentState>>,
        channel: Arc<UdpSocketChannel>,
        data_broadcaster: Arc<ChannelBroadcast<(Vec<u8>, SocketAddr)>>,
        stun_broadcaster: Arc<ChannelBroadcast<(Message, SocketAddr)>>,
    ) {
        // XXX: can we remove this demuxing task?
        // retrieve stream outside task to avoid a race
        let s = channel.receive_stream();
        async_std::task::spawn({
            async move {
                futures::pin_mut!(s);
                while let Some((data, from)) = s.next().await {
                    match Message::from_bytes(&data) {
                        Ok(msg) => {
                            debug!("received from {:?} {}", from, msg);
                            let handle = {
                                let mut state = state.lock().unwrap();
                                state.handle_stun(msg.clone(), from)
                            };
                            match handle {
                                HandleStunReply::Reply(msg) => {
                                    let buf = msg.to_bytes();
                                    let _ = channel.send_to(&buf, from).await;
                                }
                                HandleStunReply::Broadcast(msg) => {
                                    stun_broadcaster.broadcast((msg, from)).await;
                                }
                                HandleStunReply::Failure(err) => {
                                    error!("Failed to handle {}. {:?}", msg, err);
                                }
                                _ => {}
                            }
                        }
                        Err(_) => data_broadcaster.broadcast((data, from)).await,
                    }
                }
            }
        });
    }

    fn ensure_receive_task_loop(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if !state.receive_loop_started {
                StunAgent::receive_task_loop(
                    self.state.clone(),
                    self.channel.clone(),
                    self.data_broadcaster.clone(),
                    self.stun_broadcaster.clone(),
                );
                state.receive_loop_started = true;
            }
        }
    }

    pub fn data_receive_stream_filter<F>(
        &self,
        filter: F,
    ) -> impl Stream<Item = (Vec<u8>, SocketAddr)>
    where
        F: Fn(&(Vec<u8>, SocketAddr)) -> bool + Send + Sync + 'static,
    {
        self.ensure_receive_task_loop();
        self.data_broadcaster.channel_with_filter(filter)
    }

    pub fn data_receive_stream(&self) -> impl Stream<Item = (Vec<u8>, SocketAddr)> {
        self.data_receive_stream_filter(|_| true)
    }

    pub fn stun_receive_stream_filter<F>(
        &self,
        filter: F,
    ) -> impl Stream<Item = (Message, SocketAddr)>
    where
        F: Fn(&(Message, SocketAddr)) -> bool + Send + Sync + 'static,
    {
        self.ensure_receive_task_loop();
        self.stun_broadcaster.channel_with_filter(filter)
    }

    pub fn stun_receive_stream(&self) -> impl Stream<Item = (Message, SocketAddr)> {
        self.stun_receive_stream_filter(|_| true)
    }

    async fn send_request(
        &self,
        msg: &Message,
        recv_abort_handle: AbortHandle,
        to: SocketAddr,
    ) -> std::io::Result<()> {
        // FIXME: configurable timeout values: RFC 4389 Secion 7.2.1
        let timeouts: [u64; 7] = [0, 500, 1500, 3500, 7500, 15500, 31500];
        for timeout in timeouts.iter() {
            Delay::new(Duration::from_millis(timeout.clone())).await;
            info!("sending {}", msg);
            let buf = msg.to_bytes();
            trace!("sending {:?}", buf);
            self.channel.send_to(&buf, to).await?;
        }

        // on failure, abort the receiver waiting
        recv_abort_handle.abort();
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "request timed out",
        ))
    }

    pub async fn stun_request_transaction(
        &self,
        msg: &Message,
        addr: SocketAddr,
    ) -> Result<(Message, SocketAddr), AgentError> {
        if !msg.has_class(MessageClass::Request) {
            return Err(AgentError::WrongImplementation);
        }
        Self::maybe_store_message(self.state.clone(), msg.clone());
        let tid = msg.transaction_id();
        let (recv_abort_handle, recv_registration) = futures::future::AbortHandle::new_pair();
        let (send_abortable, send_abort_handle) =
            futures::future::abortable(self.send_request(&msg, recv_abort_handle, addr));

        let mut receive_s = self
            .stun_receive_stream_filter(move |(incoming, _from)| tid == incoming.transaction_id());
        let recv_abortable = futures::future::Abortable::new(
            receive_s.next().then(|msg| async move {
                send_abort_handle.abort();
                msg
            }),
            recv_registration,
        );

        futures::pin_mut!(recv_abortable);
        futures::pin_mut!(send_abortable);

        // race the sending and receiving futures returning the first that succeeds
        match futures::future::try_select(send_abortable, recv_abortable).await {
            Ok(Either::Left((x, _))) => x.map(|_| (Message::new_error(msg), addr)),
            Ok(Either::Right((y, _))) => y.ok_or(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Stun Request timed out",
            )),
            Err(_) => unreachable!(),
        }
        .map_err(|e| AgentError::IoError(e))
    }
}

#[derive(Debug)]
enum HandleStunReply {
    Reply(Message),
    Broadcast(Message),
    Failure(AgentError),
    Ignore,
}
impl From<AgentError> for HandleStunReply {
    fn from(e: AgentError) -> Self {
        HandleStunReply::Failure(e)
    }
}

impl StunAgentState {
    fn new() -> Self {
        Self {
            outstanding_requests: HashMap::new(),
            local_credentials: None,
            remote_credentials: None,
            receive_loop_started: false,
        }
    }

    fn handle_binding_request(msg: &Message, from: SocketAddr) -> Result<Message, AgentError> {
        if let Some(error) = Message::check_attribute_types(
            msg,
            &[
                USERNAME,
                XOR_MAPPED_ADDRESS,
                FINGERPRINT,
                MESSAGE_INTEGRITY,
                ICE_CONTROLLED,
                ICE_CONTROLLING,
                PRIORITY,
            ],
            &[XOR_MAPPED_ADDRESS],
        ) {
            return Ok(error);
        }
        // TODO: validate integrity

        let mapped_address =
            XorMappedAddress::from_raw(msg.get_attribute(XOR_MAPPED_ADDRESS).unwrap()).ok();
        if let None = mapped_address {
            debug!("Message is missing XOR-MAPPED-ADDRESS");
            return Message::bad_request(msg);
        }

        let mut response = Message::new_success(msg);
        response.add_attribute(XorMappedAddress::new(from, msg.transaction_id())?.to_raw())?;
        Ok(response)
    }

    fn handle_stun(&mut self, msg: Message, from: SocketAddr) -> HandleStunReply {
        // TODO: validate message with credentials
        if msg.is_response() {
            if let Some(_orig_request) = self.outstanding_requests.remove(&msg.transaction_id()) {
                return HandleStunReply::Broadcast(msg);
            } else {
                trace!("unmatched stun response, dropping");
                // unmatched response -> drop
                return HandleStunReply::Ignore;
            }
        } else if msg.has_class(MessageClass::Request) {
            if msg.has_method(BINDING) {
                return match StunAgentState::handle_binding_request(&msg, from)
                    .or_else(|_| Message::bad_request(&msg))
                {
                    Ok(response) => HandleStunReply::Reply(response),
                    Err(e) => HandleStunReply::Failure(e),
                };
            }
        }
        HandleStunReply::Broadcast(msg)
    }
}
