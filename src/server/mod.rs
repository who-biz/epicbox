use colored::*;
use futures::{
    future::lazy,
    sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    Future, Stream,
};
use std::collections::HashMap;
use uuid::Uuid;

use ws::{CloseCode, Handler, Handshake, Message, Request, Response, Result as WsResult, Sender, connect};

use epicboxlib::error::{ErrorKind, Result};
use epicboxlib::types::{EpicboxAddress, EpicboxError, EpicboxRequest, EpicboxResponse};
use epicboxlib::utils::crypto::{verify_signature, Base58, Hex};
use epicboxlib::utils::secp::{PublicKey, Signature};

use crate::broker::{BrokerRequest, BrokerResponse};

static MAX_SUBSCRIPTIONS: usize = 1;

pub struct BrokerResponseHandler {
    inner: std::sync::Arc<std::sync::Mutex<Server>>,
    response_receiver: UnboundedReceiver<BrokerResponse>,
}

pub struct AsyncServer {
    id: String,
    inner: std::sync::Arc<std::sync::Mutex<Server>>,
    nats_sender: UnboundedSender<BrokerRequest>,
    response_handlers_sender: UnboundedSender<BrokerResponseHandler>,
    subscriptions: HashMap<String, Subscription>,
    epicbox_domain: String,
    epicbox_port: u16,
    epicbox_protocol_unsecure: bool,
}

pub struct Server {
    id: String,
    out: Sender,
}

struct Subscription {}

#[derive(Serialize, Deserialize, Debug)]
struct SignedPayload {
    str: String,
    challenge: String,
    signature: String,
}

impl Drop for AsyncServer {
    fn drop(&mut self) {
        for (subject, _subscription) in &self.subscriptions {
            if self
                .nats_sender
                .unbounded_send(BrokerRequest::Unsubscribe {
                    id: self.id.clone(),
                })
                .is_err()
            {
                error!("failed to unsubscribe while dropping server!");
            };
        }
    }
}

impl AsyncServer {
    pub fn new(
        out: Sender,
        nats_sender: UnboundedSender<BrokerRequest>,
        response_handlers_sender: UnboundedSender<BrokerResponseHandler>,
        epicbox_domain: &str,
        epicbox_port: u16,
        epicbox_protocol_unsecure: bool,
    ) -> AsyncServer {
        let id = Uuid::new_v4().to_string();

        let server = Server {
            id: id.clone(),
            out,
        };

        AsyncServer {
            id: id.clone(),
            inner: std::sync::Arc::new(std::sync::Mutex::new(server)),
            nats_sender,
            response_handlers_sender,
            subscriptions: HashMap::new(),
            epicbox_domain: epicbox_domain.to_string(),
            epicbox_port,
            epicbox_protocol_unsecure,
        }
    }

    pub fn init() -> UnboundedSender<BrokerResponseHandler> {
        let (fut_tx, fut_rx) = unbounded::<BrokerResponseHandler>();

        std::thread::spawn(move || {
            info!("broker handler started");
            let fut_loop = fut_rx
                .for_each(move |handler| {
                    let clone = handler.inner.clone();
                    let response_loop = handler.response_receiver.for_each(move |m| {
                        match m {
                            BrokerResponse::Message {
                                subject: _,
                                payload,
                                reply_to,
                            } => {
                                let signed_payload =
                                    serde_json::from_str::<SignedPayload>(&payload);
                                if signed_payload.is_ok() {
                                    let signed_payload = signed_payload.unwrap();
                                    let response = EpicboxResponse::Slate {
                                        from: reply_to,
                                        str: signed_payload.str,
                                        challenge: signed_payload.challenge,
                                        signature: signed_payload.signature,
                                    };
                                    let guard = clone.lock().unwrap();
                                    let ref server = *guard;
                                    info!("[{}] <- {}", server.id.bright_green(), response);
                                    if server
                                        .out
                                        .send(serde_json::to_string(&response).unwrap())
                                        .is_err()
                                    {
                                        error!("failed sending slate to client!");
                                    };
                                } else {
                                    error!("invalid payload!");
                                }
                            }
                        }
                        Ok(())
                    });

                    std::thread::spawn(move || {
                        tokio::run(lazy(|| {
                            tokio::spawn(response_loop);
                            Ok(())
                        }));
                    });
                    Ok(())
                })
                .map_err(|_| {});

            tokio::run(lazy(move || tokio::spawn(fut_loop)));
            debug!("future thread ended...");
        });
        fut_tx
    }

    fn error(kind: EpicboxError) -> EpicboxResponse {
        let description = format!("{}", kind);
        EpicboxResponse::Error { kind, description }
    }

    fn ok() -> EpicboxResponse {
        EpicboxResponse::Ok
    }

    fn get_challenge_raw(&self) -> &str {
        "7WUDtkSaKyGRUnQ22rE3QUXChV8DmA6NnunDYP4vheTpc"
    }

    fn get_challenge(&self) -> EpicboxResponse {
        EpicboxResponse::Challenge {
            str: String::from(self.get_challenge_raw()),
        }
    }

    fn verify_signature(&self, public_key: &str, challenge: &str, signature: &str) -> Result<()> {
        let (public_key, _) = PublicKey::from_base58_check_raw(public_key, 2)?;
        let signature = Signature::from_hex(signature)?;
        verify_signature(challenge, &signature, &public_key)
            .map_err(|_| ErrorKind::EpicboxProtocolError(EpicboxError::InvalidSignature))?;
        Ok(())
    }

    fn subscribe(&mut self, address: String, signature: String) -> EpicboxResponse {
        let result = self.verify_signature(&address, self.get_challenge_raw(), &signature);
        match result {
            Ok(()) => {
                if self.subscriptions.len() == MAX_SUBSCRIPTIONS {
                    AsyncServer::error(EpicboxError::TooManySubscriptions)
                } else {
                    let (res_tx, res_rx) = unbounded::<BrokerResponse>();
                    if self
                        .nats_sender
                        .unbounded_send(BrokerRequest::Subscribe {
                            id: self.id.clone(),
                            subject: address.clone(),
                            response_sender: res_tx,
                        })
                        .is_err()
                    {
                        error!("could not issue subscribe request!");
                        return AsyncServer::error(EpicboxError::UnknownError);
                    };

                    if self
                        .response_handlers_sender
                        .unbounded_send(BrokerResponseHandler {
                            inner: self.inner.clone(),
                            response_receiver: res_rx,
                        })
                        .is_err()
                    {
                        error!("could not register subscription handler!");
                        return AsyncServer::error(EpicboxError::UnknownError);
                    };

                    self.subscriptions.insert(address.clone(), Subscription {});

                    AsyncServer::ok()
                }
            }
            Err(_) => AsyncServer::error(EpicboxError::UnknownError),
        }
    }

    fn unsubscribe(&mut self, address: String) -> EpicboxResponse {
        let result = self.subscriptions.remove(&address);
        match result {
            Some(_subscription) => {
                if self
                    .nats_sender
                    .unbounded_send(BrokerRequest::Unsubscribe {
                        id: self.id.clone(),
                    })
                    .is_err()
                {
                    error!("could not unsubscribe!");
                    return AsyncServer::error(EpicboxError::UnknownError);
                };

                AsyncServer::ok()
            }
            None => AsyncServer::error(EpicboxError::InvalidRequest),
        }
    }

    fn post_slate(
        &self,
        from: String,
        to: String,
        str: String,
        signature: String,
        message_expiration_in_seconds: Option<u32>,
    ) -> EpicboxResponse {
        let from_address = EpicboxAddress::from_str_raw(&from);
        if from_address.is_err() {
            return AsyncServer::error(EpicboxError::InvalidRequest);
        }
        let from_address = from_address.unwrap();

        let to_address = EpicboxAddress::from_str_raw(&to);
        if to_address.is_err() {
            return AsyncServer::error(EpicboxError::InvalidRequest);
        }
        let to_address = to_address.unwrap();

	info!(">>> to_address.domain({}), to_address.port({})",to_address.domain,to_address.port);

        let mut challenge = String::new();
        challenge.push_str(&str);

        let mut result =
            self.verify_signature(&from_address.public_key, &challenge, &signature);

        let mut challenge_raw = "";
        if result.is_err() {
            challenge.push_str(self.get_challenge_raw());
            challenge_raw = self.get_challenge_raw();
            result = self.verify_signature(&from_address.public_key, &challenge, &signature);
        }

        if result.is_err() {
            return AsyncServer::error(EpicboxError::InvalidSignature);
        }

        if to_address.port == self.epicbox_port && to_address.domain == self.epicbox_domain {
            let signed_payload = SignedPayload {
                str,
                challenge: challenge_raw.to_string(),
                signature,
            };

            let signed_payload = serde_json::to_string(&signed_payload).unwrap();

            if self
                .nats_sender
                .unbounded_send(BrokerRequest::PostMessage {
                    subject: to_address.public_key,
                    payload: signed_payload,
                    reply_to: from_address.stripped(),
                    message_expiration_in_seconds,
                })
                .is_err()
                {
                    error!("could not post message to broker!");
                    return AsyncServer::error(EpicboxError::UnknownError);
                };

            AsyncServer::ok()
        } else {
            self.post_slate_federated(&from_address, &to_address, str, signature, message_expiration_in_seconds)
        }
    }

    fn post_slate_federated(&self, from_address: &EpicboxAddress, to_address: &EpicboxAddress, str: String, signature: String, message_expiration_in_seconds: Option<u32>) -> EpicboxResponse {

	let mut object: serde_json::Value = serde_json::from_str(&str).unwrap();
	info!(" >>>> to argument ({}), str argument({:?})",to_address,object);
	let domain = object["destination"]["domain"].as_str().unwrap();
	info!(" >>>> domain from json ({:?})", domain);

        let url = match self.epicbox_protocol_unsecure {
            false => format!(
                "wss://{}:{}",
                domain,
                to_address.port
            ),
            true => format!(
                "ws://{}:{}",
                domain,
                to_address.port
            )
        };

        let str = str.clone();
        let signature = signature.clone();

	info!(">>> to_address.domain({}), to_address.port({})",to_address.domain,to_address.port);

        let result = connect(url, move |sender| {
            let str = str.clone();
            let signature = signature.clone();
            move |msg: Message| {
                let response = serde_json::from_str::<EpicboxResponse>(&msg.to_string())
                    .expect("could not parse response!");

                match response {
                    EpicboxResponse::Challenge { str: _ } => {
                        let request = EpicboxRequest::PostSlate {
                            from: from_address.stripped(),
                            to: to_address.stripped(),
                            str: str.clone(),
                            signature: signature.clone(),
                            message_expiration_in_seconds,
                        };

                        sender
                            .send(serde_json::to_string(&request).unwrap())
                            .unwrap();
                    }
                    EpicboxResponse::Error {
                        kind: _,
                        description: _,
                    } => {
                        sender.close(CloseCode::Abnormal).is_ok();
                    }
                    EpicboxResponse::Ok => {
                        sender.close(CloseCode::Normal).is_ok();
                    }
                    _ => {}
                }
                Ok(())
            }
        });

        match result {
            Ok(()) => AsyncServer::ok(),
            Err(_) => AsyncServer::error(EpicboxError::UnknownError),
        }
    }
}

impl Handler for AsyncServer {
    fn on_request(&mut self, req: &Request) -> WsResult<Response> {
        let res = Response::from_request(req);
        if let Err(_) = res {
            let response = Response::new(200, "", vec![]);
            Ok(response)
        } else {
            Ok(res.unwrap())
        }
    }

    fn on_open(&mut self, _: Handshake) -> WsResult<()> {
        info!(
            "[{}] {}",
            self.id.bright_green(),
            "connection established".bright_purple()
        );

        let response = self.get_challenge();
        debug!("[{}] <- {}", self.id.bright_green(), response);
        let server = self.inner.lock().unwrap();
        if server
            .out
            .send(serde_json::to_string(&response).unwrap())
            .is_err()
        {
            error!("could not send challenge to client!");
        };
        Ok(())
    }

    fn on_message(&mut self, msg: Message) -> WsResult<()> {
        let request = serde_json::from_str(&msg.to_string());

        let response = if request.is_ok() {
        	let request = request.unwrap();
        	info!("[{}] -> {}", self.id.bright_green(), request);
            match request {
                EpicboxRequest::Challenge => self.get_challenge(),
                EpicboxRequest::Subscribe { address, signature } => {
                    self.subscribe(address, signature)
                }
                EpicboxRequest::PostSlate {
                    from,
                    to,
                    str,
                    signature,
                    message_expiration_in_seconds,
                } => {
			let mut object: serde_json::Value = serde_json::from_str(&str).unwrap();
		    	info!(" >>>> to argument ({}), str argument({:?})",to,object);
			let dest = object["destination"]["domain"].as_str();
			info!(" >>>> domain from json ({:?})", dest);
		    	self.post_slate(from, to, str, signature, message_expiration_in_seconds)
		},
                EpicboxRequest::Unsubscribe { address } => self.unsubscribe(address),
            }
        } else {
            debug!(
                "[{}] -> {}",
                self.id.bright_green(),
                "invalid request!".bright_red()
            );
            AsyncServer::error(EpicboxError::InvalidRequest)
        };

        info!("[{}] <- {}", self.id.bright_green(), response);
        let server = self.inner.lock().unwrap();
        server.out.send(serde_json::to_string(&response).unwrap())
    }

    fn on_close(&mut self, code: CloseCode, _reason: &str) {
        let code = format!("{:?}", code);
        info!(
            "[{}] {} [{}]",
            self.id.bright_green(),
            "connection dropped".bright_purple(),
            code.bright_green()
        );
    }

    fn on_error(&mut self, err: ws::Error) {
        error!("the server encountered an error: {:?}", err);
    }
}
