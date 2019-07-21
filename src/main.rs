use clap::{crate_version, App, Arg};
use error_chain::error_chain;
use futures::future::join_all;
use futures::{Future, Stream};
use log::{debug, error, info, trace};
use regex::Regex;
use reqwest::r#async::Response;
use reqwest::StatusCode;
use serde::Deserialize;
use std::str::FromStr;
use std::sync::Arc;
use tokio::codec::{FramedWrite, LinesCodec};
use tokio::fs;
use walkdir::WalkDir;

error_chain! {
    foreign_links {
        Io(std::io::Error);
        Reqwest(reqwest::Error);
    }
}
type SFuture<T> = Box<Future<Item = T, Error = Error> + Send>;

trait FutureChainErr<T> {
    fn chain_err<F, E>(self, callback: F) -> SFuture<T>
    where
        F: FnOnce() -> E + 'static + Send,
        E: Into<ErrorKind>;
}

impl<F> FutureChainErr<F::Item> for F
where
    F: Future + 'static + Send,
    F::Error: std::error::Error + Send + 'static,
    F::Item: Send,
{
    fn chain_err<C, E>(self, callback: C) -> SFuture<F::Item>
    where
        C: FnOnce() -> E + 'static + Send,
        E: Into<ErrorKind>,
    {
        Box::new(self.then(|r| r.chain_err(callback)))
    }
}

static DEFAULT_EXCLUDE: &str =
    r"/(\.eggs|\.git|\.hg|\.mypy_cache|\.nox|\.tox|\.venv|_build|buck-out|build|dist)/";
static DEFAULT_INCLUDE: &str = r"\.pyi?$";

fn send_req(
    url: &str,
    config: &BlackConfig,
    bytes: Vec<u8>,
) -> impl Future<Item = Response, Error = Error> {
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
    req.send().chain_err(|| "Error sending format request")
}

fn handle_response(resp: Response) -> SFuture<()> {
    trace!("HTTP {:?}", &resp);
    let status = resp.status();
    let mut body = resp.into_body();
    match status {
        StatusCode::OK => Box::new(handle_reformat(body)),
        StatusCode::NO_CONTENT => Box::new(futures::future::ok(())),
        other => {
            if let Ok(contents) = body.by_ref().concat2().wait() {
                debug!("Contents: {:?}", contents);
            } else {
                debug!("Couldn't read response contents.");
            }
            Box::new(futures::future::err(Error::from(format!(
                "unexpected status: {:?}",
                other
            ))))
        }
    }
}

fn handle_reformat(
    body: impl Stream<Item = reqwest::r#async::Chunk, Error = reqwest::Error> + 'static + Send,
) -> impl Future<Item = (), Error = Error> {
    let codec = LinesCodec::new_with_max_length(2048);
    let stdout = FramedWrite::new(tokio::io::stdout(), codec);
    let bodystr = body
        .and_then(|chunk| Ok(String::from_utf8(chunk.to_vec()).unwrap()))
        .map_err(Error::from);
    bodystr
        .forward(stdout)
        .chain_err(|| "Error writing output")
        .map(|_| {})
}

use std::path::Path;

fn walk_dir<P: AsRef<Path>>(
    exclude: Regex,
    include: Regex,
    dir: P,
) -> impl Future<Item = Vec<String>, Error = ()> {
    let srcs = collect_sources(exclude, include, dir.as_ref().to_str().unwrap());
    srcs.collect()
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

#[derive(Default, Deserialize)]
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

fn read_config(path: String) -> Result<BlackConfig> {
    std::fs::read_to_string(path)
        .chain_err(|| "Unable to read config file")
        .and_then(|contents| toml::from_str(&contents).chain_err(|| "Unable to parse TOML"))
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
    let mut config = config_file
        .map(|x| read_config(x).unwrap())
        .unwrap_or_default();
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

    Some((
        BlaccConfig {
            url: String::from(matches.value_of("url")?),
            quiet: matches.is_present("quiet"),
            verbosity: matches.occurrences_of("verbose") as usize,
            srcs,
        },
        config,
    ))
}

fn run() -> Result<()> {
    let (config, black_config) = make_config().chain_err(|| "Unable to set up configuration")?;
    let logging = stderrlog::new()
        .module(module_path!())
        .quiet(config.quiet)
        .verbosity(config.verbosity)
        .init()
        .chain_err(|| "Unable to set up logging");

    let exclude = Regex::new(
        black_config
            .exclude
            .as_ref()
            .chain_err(|| "no exclude regex set")?,
    )
    .chain_err(|| "exclude is not a valid regex")?;
    let include = Regex::new(
        black_config
            .include
            .as_ref()
            .chain_err(|| "no include regex set")?,
    )
    .chain_err(|| "include is not a valid regex")?;
    let srcs = config.srcs;
    let url = Arc::new(config.url);
    let black_config = Arc::new(black_config);

    debug!("blacc version {} connecting to {}", crate_version!(), url);
    debug!("Inputs: {:?}", &srcs);

    let futs = srcs.into_iter().map(move |src| {
        let url = Arc::clone(&url);
        let black_config = Arc::clone(&black_config);
        let dirs = walk_dir(exclude.to_owned(), include.to_owned(), src);
        dirs.and_then(|files| {
            join_all(files.into_iter().map(move |src| {
                let fname = Arc::new(src);
                let fname2 = Arc::clone(&fname);
                let url = Arc::clone(&url);
                let black_config = Arc::clone(&black_config);

                fs::read(fname.to_string())
                    .chain_err(|| "error opening file")
                    .and_then(move |x| send_req(&url, &black_config, x))
                    .and_then(handle_response)
                    .map_err(move |err| error!("{}: {}", &fname, &err))
                    .map(move |_| info!("{}: reformatted", &fname2))
            }))
        })
    });

    tokio::run(join_all(futs).map(|_| ()));
    logging
}

fn main() {
    if let Err(ref e) = run() {
        println!("error: {}", e);
        for e in e.iter().skip(1) {
            println!("caused by: {}", e);
        }
        if let Some(backtrace) = e.backtrace() {
            println!("backtrace: {:?}", backtrace);
        }
        std::process::exit(1);
    }
}
