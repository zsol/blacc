use clap::{crate_version, App, Arg};
use futures::future::join_all;
use futures::{Future, Stream};
use log::{debug, error, info, trace};
use reqwest::r#async::Response;
use reqwest::StatusCode;
use tokio::codec::{FramedWrite, LinesCodec};
use tokio::fs;

fn send_req(url: &str, bytes: Vec<u8>) -> impl Future<Item = Response, Error = String> {
    let client = reqwest::r#async::Client::new();
    client
        .post(url)
        .body(bytes)
        .header("X-Protocol-Version", "1")
        .send()
        .map_err(|err| format!("Error sending format request: {:?}", err))

}

fn handle_response(resp: Response) -> impl Future<Item = (), Error = String> {
    trace!("HTTP response {:?}", &resp);
    let status = resp.status();
    let mut body = resp.into_body();
    futures::future::result(match status {
        StatusCode::OK => Ok(()),
        StatusCode::NO_CONTENT => Err(String::from("already well-formatted")),
        other => {
            if let Ok(contents) = body.by_ref().concat2().wait() {
                debug!("Contents: {:?}", contents);
            } else {
                debug!("Couldn't read response contents.");
            }
            Err(format!("unexpected status: {:?}", other))
        }
    })
    .and_then(|_: ()| handle_reformat(body))
}

fn handle_reformat(
    body: impl Stream<Item = reqwest::r#async::Chunk, Error = reqwest::Error>,
) -> impl Future<Item = (), Error = String> {
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
        .version(crate_version!())
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .multiple(true)
                .help("Increase logging verbosity"),
        )
        .arg(Arg::with_name("quiet").short("q").help("Silence all logs"))
        .arg(
            Arg::with_name("url")
                .required(true)
                .long("url")
                .short("u")
                .takes_value(true)
                .help("URL of a running `blackd` server"),
        )
        .arg(
            Arg::with_name("src")
                .help("Input source to be formatted")
                .takes_value(true)
                .default_value(".")
                .multiple(true)
                .empty_values(false),
        )
        .get_matches();

    let verbosity = matches.occurrences_of("verbose") as usize;
    let quiet = matches.is_present("quiet");
    let srcs = matches.values_of("src").unwrap();
    stderrlog::new()
        .module(module_path!())
        .quiet(quiet)
        .verbosity(verbosity)
        .init()
        .unwrap();

    debug!(
        "blackc version {} connecting to {}",
        crate_version!(),
        matches.value_of("url").unwrap()
    );
    debug!("Inputs: {:?}", &srcs);

    let futs = srcs
        .map(|src| {
            let fname = src.to_string();
            let fname2 = src.to_string();
            let url = matches.value_of("url").unwrap().to_string();
            fs::read(fname.to_string())
                .map_err(|err| {
                    // TODO: handle directory case
                    format!("Error opening file: {:?}", err)
                })
                .and_then(move |x| send_req(&url, x))
                .and_then(handle_response)
                .map_err(move |err| error!("{}: {}", &fname, &err))
                .map(move |_| info!("{}: reformatted", &fname2))
                .or_else(|_| Ok(()))
        })
        .collect::<Vec<_>>();

    tokio::run(join_all(futs).map(|_| {}));
}
