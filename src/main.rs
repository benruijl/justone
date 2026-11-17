#[macro_use]
extern crate slog;
#[macro_use]
extern crate slog_scope;
#[macro_use]
extern crate serde_derive;

use clap::{App, Arg};
use justone::WordList;
use slog::{Drain, Level, LevelFilter};

pub mod justone;
pub mod server;

#[tokio::main]
async fn main() {
    // log to the terminal only (warnings and errors); no log file
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let log = slog::Logger::root(LevelFilter::new(drain, Level::Warning).fuse(), o!());

    let _guard = slog_scope::set_global_logger(log);
    slog_scope::scope(&slog_scope::logger().new(slog_o!()), || start()).await;
}

async fn start() {
    let cli_options = App::new("Just One")
        .version("0.1")
        .author("Ben Ruijl <benruyl@gmail.com>")
        .about("Just One game server")
        .arg(
            Arg::with_name("cli")
                .short("c")
                .long("use_cli")
                .help("Use CLI instead of web server"),
        )
        .arg(
            Arg::with_name("tls")
                .long("tls")
                .min_values(2)
                .max_values(2)
                .help("Use TLS: provide cert and key file as argument"),
        )
        .arg(
            Arg::with_name("port")
                .long("port")
                .short("p")
                .takes_value(true)
                .default_value("3030")
                .help("Specify the port"),
        )
        .get_matches();

    if cli_options.is_present("cli") {
        justone::JustOneGame::new_game_cli(2, WordList::English).await;
    } else {
        server::start(&cli_options).await
    }
}
