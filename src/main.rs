use clap::{crate_version, App, Arg};
use futures::stream;
use futures::{Future, Stream};
use log::{debug, error, info, trace};
use regex::Regex;
use reqwest::r#async::Response;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::codec::{FramedWrite, LinesCodec};
use tokio::fs;
use walkdir::WalkDir;
use std::sync::Arc;
use std::str::FromStr;

static DEFAULT_EXCLUDE: &str =
    r"/(\.eggs|\.git|\.hg|\.mypy_cache|\.nox|\.tox|\.venv|_build|buck-out|build|dist)/";
static DEFAULT_INCLUDE: &str = r"\.pyi?$";

fn send_req(url: &str, config: &BlackConfig, bytes: Vec<u8>) -> impl Future<Item = Response, Error = String> {
    let client = reqwest::r#async::Client::new();
    let mut req = client
        .post(url)
        .body(bytes)
        .header("X-Protocol-Version", "1");
    
    if config.pyi.unwrap_or(false) {
        req = req.header("X-Python-Variant", "pyi");
    }

    if let Some(version) = &config.target_version {
        req = req.header("X-Python-Variant", version.to_owned());
    }

    if let Some(true) = config.skip_string_normalization {
        req = req.header("X-Skip-String-Normalization", "true")
    }

    if let Some(length) = config.line_length {
        req = req.header("X-Line-Length", length)
    }

    trace!("Sending HTTP {:?}", &req);
    req
        .send()
        .map_err(|err| format!("Error sending format request: {:?}", err))
}

fn handle_response(resp: Response) -> impl Future<Item = (), Error = String> {
    trace!("HTTP {:?}", &resp);
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

fn collect_sources(
    exclude: Regex,
    include: Regex,
    dir: &str,
) -> impl Stream<Item = String, Error = ()> {
    futures::stream::iter_ok(
        WalkDir::new(dir)
            .into_iter()
            .filter_entry(move |entry| {
                entry
                    .path()
                    .to_str()
                    .map(|path| {
                        let ret = !exclude.is_match(path);
                        if !ret {
                            debug!("Ignoring {} because it matches exclude regex", path);
                        }
                        ret
                    })
                    .unwrap_or(false)
            })
            .filter_map(move |entry| {
                if let Ok(entry) = entry {
                    if entry.file_type().is_file() {
                        if let Some(path) = entry.path().to_str() {
                            if include.is_match(path) {
                                return Some(path.to_owned());
                            } else {
                                debug!("Ignoring {} because it doesn't match include regex", path);
                            }
                        }
                    }
                }
                None
            }),
    )
}

#[derive(Default)]
#[derive(Deserialize)]
struct BlackConfig {
    exclude: Option<String>,
    include: Option<String>,
    line_length: Option<u64>,
    target_version: Option<String>,
    skip_string_normalization: Option<bool>,
    pyi: Option<bool>,
    fast: Option<bool>,
}

struct BlaccConfig {
    url: String,
    quiet: bool,
    verbosity: usize,
    srcs: Vec<String>,
}

fn read_config(path: String) -> Result<BlackConfig, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}", e)).and_then(|contents| toml::from_str(&contents).map_err(|e| format!("{}", e)))
}

fn make_config() -> Option<(BlaccConfig, BlackConfig)> {
    let matches =
        App::new("Black Client")
            .version(crate_version!())
            .arg(Arg::with_name("verbose").short("-v").help("Set verbosity").multiple(true))
            .arg(Arg::with_name("quiet").short("q").help("Silence all logs").conflicts_with("quiet"))
            .arg(
                Arg::with_name("url")
                    .required(true)
                    .long("url")
                    .short("u")
                    .takes_value(true)
                    .help("URL of a running `blackd` server"),
            )
            .arg(
                Arg::with_name("exclude")
                    .long("exclude")
                    .help("A regular expression that matches files and directories that should be excluded on recursive searches")
                    .long_help("An empty value means no paths are excluded. Exclusions are calculated first, inclusions later.")
                    .takes_value(true)
                    .default_value(DEFAULT_EXCLUDE)
            )
            .arg(
                Arg::with_name("include")
                    .long("include")
                    .help("A regular expression that matches files and directories that should be included on recursive searches")
                    .long_help("An empty value means all files are included. Exclusions are calculated first, inclusions later.")
                    .takes_value(true)
                    .default_value(DEFAULT_INCLUDE)
            )
            .arg(Arg::with_name("fast").long("fast").help("Skip sanity checks").conflicts_with("safe"))
            .arg(Arg::with_name("safe").long("safe").help("Run sanity checks after formatting").conflicts_with("fast"))
            .arg(Arg::with_name("skip-string-normalization").short("S").long("skip-string-normalization").help("Don't normalize string quotes or prefixes"))
            .arg(Arg::with_name("target-version").short("t").long("target-version").takes_value(true).use_delimiter(true).help("Python versions that should be supported by Black's output.").possible_values(&["py27", "py33", "py34", "py35", "py36", "py37", "py38"]))
            .arg(Arg::with_name("line-length").short("l").long("line-length").takes_value(true).default_value("88").help("How many characters per line to allow"))
            .arg(
                Arg::with_name("config-file")
                    .help("Path to TOML file containing black's configuration")
                    .long("config-file")
                    .takes_value(true),
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
    let config_file = matches.value_of("config-file").map(|x| x.to_owned());
    let mut config = config_file.map(|x| read_config(x).unwrap()).unwrap_or(Default::default());
    let srcs: Vec<String> = matches
        .values_of("src")
        .unwrap()
        .map(|x| x.to_string())
        .collect();

    if let Some(exclude) = matches.value_of("exclude") {
        config.exclude = Some(String::from(exclude));
    }
    if let Some(include) = matches.value_of("include") {
        config.include = Some(String::from(include));
    }
    if let Some(length) = matches.value_of("line-length") {
        config.line_length = Some(u64::from_str(length).unwrap());
    }
    if matches.is_present("fast") {
        config.fast = Some(true);
    }

    Some((BlaccConfig {
        url: String::from(matches.value_of("url")?),
        quiet: matches.is_present("quiet"),
        verbosity: matches.occurrences_of("verbose") as usize,
        srcs,
    }, config))
}

fn main() {

    let (config, black_config) = make_config().unwrap();
    stderrlog::new()
        .module(module_path!())
        .quiet(config.quiet)
        .verbosity(config.verbosity)
        .init()
        .unwrap();
    
    debug!("blackc version {} connecting to {}", crate_version!(), config.url);
    debug!("Inputs: {:?}", &config.srcs);

    let exclude = Regex::new(black_config.exclude.as_ref().unwrap()).unwrap();
    let include = Regex::new(black_config.include.as_ref().unwrap()).unwrap();
    let srcs = config.srcs;
    let url = Arc::new(config.url);
    let black_config = Arc::new(black_config);

    let futs = stream::iter_ok(srcs)
        .map(move |x| collect_sources(exclude.to_owned(), include.to_owned(), &x))
        .flatten()
        .and_then(move |src| {
            let fname = Arc::new(src.to_string());
            let fname2 = Arc::clone(&fname);
            let url = Arc::clone(&url);
            let black_config = Arc::clone(&black_config);

            fs::read(fname.to_string())
                .map_err(|err| format!("Error opening file: {:?}", err))
                .and_then(move |x| send_req(&url, &black_config, x))
                .and_then(handle_response)
                .map_err(move |err| error!("{}: {}", &fname, &err))
                .map(move |_| info!("{}: reformatted", &fname2))
        })
        .collect()
        .map(|_| {});

    tokio::run(futs);
}
