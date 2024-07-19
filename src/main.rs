use clap::Parser;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

#[derive(clap::Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    from: Option<usize>,
    #[arg(short, long)]
    to: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Cli::parse();
    let from_port = args.from.expect("invalid `from` port");
    let to_port = args.to.expect("invalid `to` port");
    log::warn!("from :{:?} to {:?}", from_port, to_port);

    let listener = TcpListener::bind(format!("127.0.0.1:{:?}", to_port)).await?;
    loop {
        let (mut inbound, addr) = listener.accept().await?;
        log::debug!("request from {:}", addr);
        tokio::spawn(async move {
            let outbound = TcpStream::connect(format!("127.0.0.1:{:?}", from_port)).await.unwrap();
            let mut outbound = tokio::io::BufStream::new(outbound);

            let (mut ri, mut wi) = inbound.split();
            let (mut ro, mut wo) = outbound.get_mut().split();
            let client_to_server = async {
                tokio::io::copy(&mut ri, &mut wo).await.expect("client to server copy error");
                wo.shutdown().await
            };
            let server_to_client = async {
                tokio::io::copy(&mut ro, &mut wi).await.expect("server to client copy error");
                wi.shutdown().await
            };
            let _ = futures::future::try_join(client_to_server, server_to_client).await;
        });
    }
}