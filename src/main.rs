use {
    clap::Parser,
    jito_protos::shredstream::{
        shredstream_proxy_client::ShredstreamProxyClient, SubscribeEntriesRequest,
    },
    solana_entry::entry::Entry,
    solana_sdk::pubkey::Pubkey,
    std::{collections::HashSet, env, io, str::FromStr, time::Duration},
    tokio::time::sleep,
    tonic::{metadata::MetadataValue, transport::Endpoint, Request},
};

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, default_value_t = String::from("http://127.0.0.1:9999"))]
    shredstream_uri: String,

    #[clap(short, long)]
    x_token: Option<String>,

    /// Pubkeys to check, separated by spaces
    #[clap(short, long, num_args = 1..)]
    account_include: Option<Vec<String>>,
}

#[tokio::main]
async fn main() -> Result<(), io::Error> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();

    let keys_set: Option<HashSet<Pubkey>> = if let Some(keys) = args.account_include {
        let parsed: Result<Vec<Pubkey>, _> =
            keys.into_iter().map(|k| Pubkey::from_str(&k)).collect();

        match parsed {
            Ok(pubkeys) => Some(pubkeys.into_iter().collect()),
            Err(e) => {
                eprintln!("Invalid pubkey in keys_to_check: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    loop {
        match connect_and_stream(
            &args.shredstream_uri,
            args.x_token.as_deref(),
            keys_set.as_ref(),
        )
        .await
        {
            Ok(()) => {
                println!("Stream ended gracefully. Reconnecting...");
            }
            Err(e) => {
                eprintln!("Connection or stream error: {e}. Retrying...");
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

async fn connect_and_stream(
    endpoint: &str,
    x_token: Option<&str>,
    keys_set: Option<&HashSet<Pubkey>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::from_str(endpoint)?
        .keep_alive_while_idle(true)
        .http2_keep_alive_interval(Duration::from_secs(5))
        .keep_alive_timeout(Duration::from_secs(10))
        .tcp_keepalive(Some(Duration::from_secs(15)))
        .connect_timeout(Duration::from_secs(5));

    let channel = endpoint.connect().await?;
    let mut client = ShredstreamProxyClient::new(channel);

    let mut request = Request::new(SubscribeEntriesRequest {});
    if let Some(token) = x_token {
        let metadata_value = MetadataValue::from_str(token)?;
        request.metadata_mut().insert("x-token", metadata_value);
    }

    let ctx = zmq::Context::new();
    let zmq_socket = match ctx.socket(zmq::SocketType::PUSH) {
        Ok(socket) => socket,
        Err(e) => return Err(Box::new(e)),
    };
    match zmq_socket.connect("tcp://0.0.0.0:5557") {
        Ok(_) => {}
        Err(e) => return Err(Box::new(e)),
    };

    let mut stream = client.subscribe_entries(request).await?.into_inner();

    while let Some(result) = stream.message().await.transpose() {
        match result {
            Ok(slot_entry) => {
                let recv_ts_us = unix_ts_us();
                let entries = match bincode::deserialize::<Vec<Entry>>(&slot_entry.entries) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("Deserialization failed: {e}");
                        continue;
                    }
                };

                entries.iter().for_each(|e| {
                    e.transactions.iter().for_each(|t| {
                        let accounts = t.message.static_account_keys();

                        let should_print = match keys_set {
                            // No keys provided → always print
                            None => true,
                            // Keys provided → print only if any matches
                            Some(s) => accounts.iter().any(|key| s.contains(key)),
                        };
                        let raw_tx_str = serde_json::json!({
                            "recv_ts_us": recv_ts_us,
                            "txn_signatures": t.signatures
                        })
                        .to_string();
                        let topic = "shred_stream";
                        let payload = format!("{topic} {raw_tx_str}");
                        let _ = zmq_socket.send(&payload, zmq::DONTWAIT);

                        // if should_print {
                        //     println!("Times {}", recv_ts_us);
                        //     println!("Transaction: {:?}\n", t.signatures);
                        // }
                    });
                });
            }
            Err(e) => {
                eprintln!("stream error: {e}");
                return Err(Box::new(e));
            }
        }
    }

    Ok(())
}

#[allow(clippy::expect_used)]
pub fn unix_ts_us() -> u64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .expect("System time is ealier than 1970-01-01")
        .as_micros() as u64
}
