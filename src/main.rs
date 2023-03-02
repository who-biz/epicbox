#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate log;
extern crate colored;
extern crate env_logger;
extern crate failure;
#[macro_use]
extern crate futures;
extern crate bytes;
extern crate nitox;
extern crate nom;
extern crate serde_json;
extern crate tokio;
extern crate tokio_codec;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_timer;
extern crate unicode_segmentation;
extern crate uuid;
extern crate ws;

extern crate epicboxlib;

mod broker;
mod server;

use broker::Broker;
use server::AsyncServer;
use std::net::ToSocketAddrs;

fn main() {
    env_logger::init();


    let broker_uri = std::env::var("BROKER_URI")
        .unwrap_or_else(|_| "127.0.0.1:61613".to_string())
        .to_socket_addrs()
        .unwrap()
        .next();

    let username = std::env::var("BROKER_USERNAME").unwrap_or("guest".to_string());
    let password = std::env::var("BROKER_PASSWORD").unwrap_or("guest".to_string());

    let epicbox_domain = std::env::var("EPICBOX_DOMAIN").unwrap_or("127.0.0.1".to_string());
    let epicbox_port = std::env::var("EPICBOX_PORT").unwrap_or("443".to_string());
    let epicbox_port = u16::from_str_radix(&epicbox_port, 10).expect("invalid EPICBOX_PORT given!");
    let epicbox_max_connections = std::env::var("EPICBOX_MAX_CONNECTIONS").unwrap_or("1000".to_string());
    let epicbox_max_connections = usize::from_str_radix(&epicbox_max_connections, 10).expect("invalid EPICBOX_MAX_CONNECTIONS given!");
    let epicbox_protocol_unsecure = std::env::var("EPICBOX_PROTOCOL_UNSECURE")
        .map(|_| true)
        .unwrap_or(false);

    if broker_uri.is_none() {
        error!("could not resolve broker uri!");
        panic!();
    }

    let broker_uri = broker_uri.unwrap();

    let bind_address =
        std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3423".to_string());

    info!("Broker URI: {}", broker_uri);
    info!("Bind address: {}", bind_address);
    info!("Max Connections {}", epicbox_max_connections);

    let mut broker = Broker::new(broker_uri, username, password);
    let sender = broker.start().expect("failed initiating broker session");
    let response_handlers_sender = AsyncServer::init();


    ws::Builder::new().with_settings(ws::Settings {
	max_connections: epicbox_max_connections,
	..ws::Settings::default()
	})
        .build(|out| {
            AsyncServer::new(
                out,
                sender.clone(),
                response_handlers_sender.clone(),
                &epicbox_domain,
                epicbox_port,
                epicbox_protocol_unsecure,
            )
        })
        .unwrap()
        .listen(&bind_address[..])
        .unwrap();
}
