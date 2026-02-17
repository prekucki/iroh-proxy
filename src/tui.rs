use std::io::{self, Write};

use anyhow::Result;

use crate::control;

pub async fn run_tui() -> Result<()> {
    loop {
        println!();
        println!("iroh-proxy TUI");
        println!("1) Status");
        println!("2) List active connections");
        println!("3) Exit");
        print!("Select option: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim() {
            "1" => show_status().await?,
            "2" => show_connections().await?,
            "3" => break,
            _ => println!("invalid option"),
        }
    }
    Ok(())
}

async fn show_status() -> Result<()> {
    match control::status().await? {
        Some(status) => {
            println!("running: true");
            println!("endpoint: {}", status.endpoint_id);
            println!("connections: {}", status.connections);
            println!("served: {}", status.served);
            println!("forwards: {}", status.forwards);
        }
        None => {
            println!("running: false");
        }
    }
    Ok(())
}

async fn show_connections() -> Result<()> {
    let conns = control::list_connections().await?;
    if conns.is_empty() {
        println!("no active connections");
        return Ok(());
    }

    println!("{:<66}  {:<8}  dst", "src", "type");
    for conn in conns {
        println!("{:<66}  {:<8}  {}", conn.src, conn.kind, conn.dst);
    }
    Ok(())
}
