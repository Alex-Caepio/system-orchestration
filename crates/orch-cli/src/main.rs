#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match orch_cli::dispatch(&args).await {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}
