use std::net::SocketAddr;

use clap::Parser;
use thrennix_testserver::{generate_cert, serve, serve_tls, ServerConfig};

/// Reproducible HTTP(S) test server for download-manager benchmarks.
#[derive(Parser, Debug)]
#[command(name = "thrennix-testserver", version)]
struct Args {
    /// Port to listen on.
    #[arg(long, default_value_t = 8090)]
    port: u16,
    /// Address to bind (localhost only by default).
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Serve HTTPS with a freshly generated self-signed certificate.
    #[arg(long)]
    tls: bool,
    /// Do not log one line per request.
    #[arg(long, short)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse()?;
    let config = ServerConfig { log: !args.quiet };

    if args.tls {
        let dir = std::env::temp_dir().join("thrennix-testserver");
        let cert = generate_cert(&dir)?;
        println!("TLS certificate: {}", cert.cert_pem_path.display());
        println!("TLS private key: {}", cert.key_pem_path.display());
        let listener = std::net::TcpListener::bind(addr)?;
        println!(
            "thrennix-testserver listening on https://{}",
            listener.local_addr()?
        );
        serve_tls(listener, config, &cert).await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        println!(
            "thrennix-testserver listening on http://{}",
            listener.local_addr()?
        );
        serve(listener, config).await?;
    }
    Ok(())
}
