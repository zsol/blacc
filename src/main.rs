use clap::{crate_version, App, Arg};
use futures::stream;
use futures::{Future, Stream};
use log::{debug, error, info, trace};
use reqwest::r#async::Response;
use reqwest::StatusCode;
use tokio::codec::{FramedWrite, LinesCodec};
use tokio::fs;
use walkdir::WalkDir;

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

fn collect_sources(dir: &str) -> impl Stream<Item = String, Error = ()> {
    futures::stream::iter_ok(WalkDir::new(dir).into_iter().filter_map(|entry| {
        if let Ok(entry) = entry {
            if entry.file_type().is_file() {
                return Some(entry.path().to_str().unwrap().to_owned());
            }
        }
        None
    }))
}

fn main() {
    let matches = Box::new(
        App::new("Black Client")
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
            .get_matches(),
    );

    // TODO: cant figure out how to move this into futs
    let matches = Box::leak(matches);

    let verbosity = matches.occurrences_of("verbose") as usize;
    let quiet = matches.is_present("quiet");
    let url = matches.value_of("url").unwrap().to_owned();
    let srcs = matches.values_of("src").unwrap();
    stderrlog::new()
        .module(module_path!())
        .quiet(quiet)
        .verbosity(verbosity)
        .init()
        .unwrap();

    debug!("blackc version {} connecting to {}", crate_version!(), url,);
    debug!("Inputs: {:?}", &srcs);

    let futs = stream::iter_ok(srcs.map(collect_sources))
        .flatten()
        .and_then(move |src| {
            let fname = src.to_string();
            let fname2 = src.to_string();
            let url = url.to_owned();
            fs::read(fname.to_string())
                .map_err(|err| format!("Error opening file: {:?}", err))
                .and_then(move |x| send_req(&url, x))
                .and_then(handle_response)
                .map_err(move |err| error!("{}: {}", &fname, &err))
                .map(move |_| info!("{}: reformatted", &fname2))
                .or_else(|_| Ok(()))
        })
        .collect()
        .map(|_| {});

    tokio::run(futs);
}
