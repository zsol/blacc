use clap::{App, Arg};
use futures::future::join_all;
use futures::{Future, Stream};
use reqwest::r#async::{Decoder, Response};
use reqwest::StatusCode;
use tokio::codec::{FramedWrite, LinesCodec};
use tokio::fs;

fn send_req(bytes: Vec<u8>) -> impl Future<Item = Response, Error = String> {
    let url = "http://localhost:8000/";
    let client = reqwest::r#async::Client::new();
    client
        .post(url)
        .body(bytes)
        .header("X-Protocol-Version", "1")
        .send()
        .map_err(|err| format!("Error sending format request: {:?}", err))

}

fn handle_response(resp: Response) -> impl Future<Item = (), Error = String> {
    let status = resp.status();
    let body = resp.into_body();
    futures::future::result(match status {
        StatusCode::OK => Ok(()),
        StatusCode::NO_CONTENT => Err(String::from("already well-formatted")),
        other => Err(format!("unexpected status: {:?}", other)),
    })
    .and_then(|_: ()| handle_reformat(body))
}

fn handle_reformat(body: Decoder) -> impl Future<Item = (), Error = String> {
    let codec = LinesCodec::new_with_max_length(2048);
    let stdout = FramedWrite::new(tokio::io::stdout(), codec);
    let bodystr = body
        .and_then(|chunk| Ok(String::from_utf8(chunk.to_vec()).unwrap()))
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err));
    bodystr
        .forward(stdout)
        .map_err(|err| format!("Error writing output: {:?}", err))
        .map(|_| {})
}

fn main() {
    let matches = App::new("Black Client")
        .version("0.1")
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .help("Outputs debug logs to stderr"),
        )
        .arg(
            Arg::with_name("src")
                .help("Input source to be formatted")
                .default_value(".")
                .multiple(true)
                .empty_values(false),
        )
        .get_matches();
    if matches.is_present("verbose") {
        eprintln!("Debug mode on");
    }

    let futs = matches
        .values_of("src")
        .unwrap()
        .map(|src| {
            let fname = String::from(src);
            let fname2 = String::from(src);
            fs::read(fname.to_string())
                .map_err(|err| {
                    // TODO: handle directory case
                    format!("Error opening file: {:?}", err)
                })
                .and_then(send_req)
                .and_then(handle_response)
                .map_err(move |err| eprintln!("{}: {}", fname, err))
                .map(move |_| eprintln!("{}: reformatted", fname2))
                .or_else(|_| Ok(()))
        })
        .collect::<Vec<_>>();

    tokio::run(join_all(futs).map(|_| {}));
}
