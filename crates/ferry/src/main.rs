// Release builds have no console window; CLI flags attach to the parent's console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod ipc_server;
mod log;
mod trip;
mod update;
mod vpn;
mod win;

use ferry_core::ipc::{self, Request};
use ferry_core::location::{classify, locate, reason_text};
use std::path::Path;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // The windows run in their own process so the engine never loads the renderer.
    if let Some(arg @ ("--flyout" | "--setup")) = args.get(1).map(String::as_str) {
        exit(ferry_ui::run(arg));
    }
    if args.len() > 1 {
        win::attach_console();
    }
    match args.get(1).map(String::as_str) {
        None => exit(engine::run()),
        Some("--where") => print_where(args.get(2).map(Path::new)),
        // Debug aid: the running engine's Status as JSON.
        Some("--status") => match ipc::call(&ipc::pipe(), &Request::Status) {
            Ok(r) => print!("{}", String::from_utf8_lossy(&ipc::encode(&r))),
            Err(e) => {
                eprintln!("ferry --status: {e}");
                exit(1);
            }
        },
        Some("--toast-test") => {
            let r = win::toast::ensure_shortcut().and_then(|()| {
                win::toast::toast("Ferry", "Notifications are working.").map_err(std::io::Error::other)
            });
            if let Err(e) = r {
                eprintln!("ferry --toast-test: {e}");
                exit(1);
            }
        }
        Some(other) => {
            eprintln!("ferry: unknown option {other} (try --where, --status, --toast-test)");
            exit(2);
        }
    }
}

/// `ferry --where [config.toml]`: list adapters and the resolved location.
fn print_where(config: Option<&Path>) {
    let adapters = match win::net::adapters() {
        Ok(a) => a,
        Err(e) => return eprintln!("adapters: {e}"),
    };
    for a in &adapters {
        let macs: Vec<String> = a
            .gateway_macs
            .iter()
            .map(|m| m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
            .collect();
        let prefixes: Vec<String> = a.prefixes.iter().map(|(ip, len)| format!("{ip}/{len}")).collect();
        println!(
            "#{} {:?} ({}) up={} kind={:?} if_type={} hardware={}\n    prefixes=[{}]\n    gateway_macs=[{}]\n    network={:?}",
            a.index,
            a.alias,
            a.description,
            a.up,
            classify(a),
            a.if_type,
            a.hardware,
            prefixes.join(", "),
            macs.join(", "),
            win::net::network_name(a.index),
        );
    }
    let cfg = match config.map(ferry_core::config::load).transpose() {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => return eprintln!("config: {e}"),
    };
    let l = locate(&cfg, &adapters, &win::net::best_interface, &win::net::probe_445, &vpn::check);
    println!("location: {:?}{}", l.location, if config.is_none() { " (no config given)" } else { "" });
    if let (Some(ip), Some(bulk)) = (l.server_ip, &l.bulk) {
        println!("server: {ip}  network: {:?}\nbulk: {bulk:?}", l.network_name);
        if let Some(why) = reason_text(bulk, ip) {
            println!("not copying: {why}");
        }
    }
}
