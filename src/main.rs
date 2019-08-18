use bytes::Bytes;
use clap::{crate_version, App, Arg};
use error_chain::{error_chain, ChainedError};
use futures::{
    future::{self, join_all, Either},
    sink::Sink,
    stream, Future, Stream,
};
use log::{debug, error, info, trace, warn};
use regex::{Regex, RegexBuilder};
use reqwest::{r#async::Response, StatusCode};
use serde::Deserialize;
use std::{
    borrow::Borrow,
    fmt::{Display, Formatter},
    io::Read,
    path::Path,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    codec::{BytesCodec, FramedWrite},
    fs,
    io::AsyncWrite,
};
use walkdir::WalkDir;

error_chain! {
    foreign_links {
        Io(std::io::Error);
        Reqwest(reqwest::Error);
    }
}
type SFuture<T> = Box<dyn Future<Item = T, Error = Error> + Send>;

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

#[test]
fn test_future_chain_err() {
    use std::io;
    let mut runtime = tokio::runtime::Runtime::new().unwrap();
    let f = runtime.block_on(future::ok::<(), io::Error>(()).chain_err(|| "hello"));
    assert!(f.is_ok());

    let g = runtime.block_on(
        future::err::<(), _>(io::Error::from(io::ErrorKind::Other))
            .chain_err(|| "hello"),
    );
    assert!(g.is_err());
    if let Err(err) = g {
        assert_eq!(err.description(), "hello");
    }
}

#[cfg(not(windows))]
fn get_fd_limit() -> Result<usize> {
    use std::convert::TryFrom;

    let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
    let ret = unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit as *mut libc::rlimit)
    };
    if ret == 0 {
        usize::try_from(limit.rlim_cur).or_else(|_| Ok(std::usize::MAX))
    } else {
        Err(Error::with_chain(
            std::io::Error::last_os_error(),
            "Unable to get file descriptor limit",
        ))
    }
}

#[cfg(windows)]
fn get_fd_limit() -> Result<usize> {
    // TODO
    Ok(100)
}

static FD_LIMIT: AtomicUsize = AtomicUsize::new(100);

static DEFAULT_EXCLUDE: &str =
    r"/(\.eggs|\.git|\.hg|\.mypy_cache|\.nox|\.tox|\.venv|_build|buck-out|build|dist)/";
static DEFAULT_INCLUDE: &str = r"\.pyi?$";

#[derive(Debug, PartialEq)]
enum ActionTaken {
    Reformatted,
    Noop,
}

impl Display for ActionTaken {
    fn fmt(
        self: &Self,
        f: &mut Formatter<'_>,
    ) -> std::result::Result<(), std::fmt::Error> {
        match self {
            ActionTaken::Reformatted => f.write_str("reformatted"),
            ActionTaken::Noop => f.write_str("already well-formatted"),
        }
    }
}

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
        req = req.header("X-Python-Variant", version.join(","));
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

#[test]
fn test_handle_response_error() {
    use hyper;
    use reqwest::{r#async::ResponseBuilderExt, Url};

    let mut rt = tokio::runtime::Runtime::new().unwrap();
    let resp = Response::from(
        hyper::Response::builder()
            .status(404)
            .url(Url::parse("https://lol").unwrap())
            .body("some error")
            .unwrap(),
    );
    let action = rt.block_on(handle_response(resp, None));
    assert!(action.is_err());
}

#[test]
fn test_handle_response_noop() {
    use hyper;
    use reqwest::{r#async::ResponseBuilderExt, Url};

    let mut rt = tokio::runtime::Runtime::new().unwrap();
    let resp = Response::from(
        hyper::Response::builder()
            .status(StatusCode::NO_CONTENT)
            .url(Url::parse("https://lol").unwrap())
            .body("")
            .unwrap(),
    );
    let action = rt.block_on(handle_response(resp, None));
    assert!(action.is_ok());
    assert_eq!(action.unwrap(), ActionTaken::Noop);
}

#[test]
fn test_handle_response_reformat() {
    use hyper;
    use reqwest::{r#async::ResponseBuilderExt, Url};

    let mut rt = tokio::runtime::Runtime::new().unwrap();
    let resp = Response::from(
        hyper::Response::builder()
            .status(StatusCode::OK)
            .url(Url::parse("https://lol").unwrap())
            .body("hello")
            .unwrap(),
    );
    let action = rt.block_on(handle_response(resp, None));
    assert!(action.is_ok());
    assert_eq!(action.unwrap(), ActionTaken::Reformatted);
}

fn handle_response(resp: Response, fname: Option<Arc<String>>) -> SFuture<ActionTaken> {
    trace!("HTTP {:?}", &resp);
    let status = resp.status();
    let mut body = resp.into_body();
    match status {
        StatusCode::OK => Box::new(
            handle_reformat(body, fname.clone()).map(|_| ActionTaken::Reformatted),
        ),
        StatusCode::NO_CONTENT => Box::new(future::ok(ActionTaken::Noop)),
        other => {
            if let Ok(contents) = body.by_ref().concat2().wait() {
                debug!("Contents: {:?}", contents);
            } else {
                debug!("Couldn't read response contents.");
            }
            Box::new(future::err(Error::from(format!(
                "unexpected status: {:?}",
                other
            ))))
        }
    }
}

#[test]
fn test_drain() {
    use tempfile::tempdir;
    let mut rt = tokio::runtime::Runtime::new().unwrap();
    let input = "hello".as_bytes();
    let input_stream = stream::repeat(input).take(2);
    let temp = tempdir().unwrap();
    let output = rt
        .block_on(fs::File::create(temp.path().join("lol")))
        .unwrap();

    rt.block_on(drain(input_stream, output)).unwrap();
    let buf = std::fs::read(temp.path().join("lol")).unwrap();
    assert_eq!(buf.as_slice(), "hellohello".as_bytes());
}

fn drain<In, Out>(in_stream: In, out: Out) -> impl Future<Item = (), Error = Error>
where
    In: Stream<Error = Error>,
    Out: AsyncWrite,
    In::Item: AsRef<[u8]>,
{
    let codec = BytesCodec::new();
    let sink = FramedWrite::new(out, codec)
        .sink_map_err(|e| Error::with_chain(e, "Error writing output"));
    let instr = in_stream
        .map_err(Error::from)
        .map(|chunk| Bytes::from(chunk.as_ref()));
    instr.forward(sink).map(|_| ())
}

fn handle_reformat(
    body: impl Stream<Item = reqwest::r#async::Chunk, Error = reqwest::Error>
        + 'static
        + Send,
    fname: Option<Arc<String>>,
) -> impl Future<Item = (), Error = Error> {
    let body = body.map_err(|e| Error::with_chain(e, "Error reading HTTP response"));
    match fname {
        Some(fname) => Either::A(
            tokio::fs::file::File::create(fname.as_ref().clone())
                .map_err(|e| Error::with_chain(e, "Unable to open file for writing"))
                .and_then(|f| drain(body, f)),
        ),
        None => Either::B(
            futures::future::ok::<_, ()>(tokio::io::stdout())
                .map_err(|_| Error::from("Unable to open stdout for writing"))
                .and_then(|f| drain(body, f)),
        ),
    }
}

#[test]
fn test_walk_dir() {
    use std::fs::{create_dir, File};
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let exclude_dir = root.join("exclude");
    let some_dir = root.join("some_dir");
    let exclude_file = exclude_dir.join("include.file");
    let include_file = root.join("include.file");
    let include_nested_file = some_dir.join("include.file");

    for dir in [exclude_dir, some_dir].iter() {
        create_dir(dir).unwrap();
    }
    for f in [&exclude_file, &include_file, &include_nested_file].iter() {
        File::create(f).unwrap();
    }
    let mut runtime = tokio::runtime::Runtime::new().unwrap();

    let f = walk_dir(
        Regex::from_str(r"exclude").unwrap(),
        Regex::from_str(r"include\.file").unwrap(),
        root.to_str().unwrap().to_string(),
    );

    let result = runtime.block_on(f).unwrap();
    assert_eq!(result.len(), 2);
    assert!(result.contains(&include_file.to_str().unwrap().to_owned()));
}

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

#[derive(Debug, Default, Deserialize)]
struct BlackConfig {
    exclude: Option<String>,
    include: Option<String>,
    line_length: Option<u64>,
    target_version: Option<Vec<String>>,
    skip_string_normalization: Option<bool>,
    pyi: Option<bool>,
    fast: Option<bool>,
}

#[derive(Debug)]
struct BlaccConfig {
    url: String,
    quiet: bool,
    verbosity: usize,
    srcs: Vec<String>,
}

fn read_config(path: String) -> Result<BlackConfig> {
    std::fs::read_to_string(path)
        .chain_err(|| "Unable to read config file")
        .and_then(|contents| {
            let v = toml::from_str::<toml::Value>(&contents)
                .chain_err(|| "Unable to parse TOML")?;
            v.get("tool")
                .ok_or(Error::from("No `tool` section in config"))
                .and_then(|tool| {
                    tool.get("black")
                        .ok_or(Error::from("No `tool.black` section in config"))
                        .and_then(|raw_config| {
                            let mut raw_config = raw_config.to_owned();
                            let table = raw_config
                                .as_table_mut()
                                .ok_or(Error::from("`tool.black` is not a table"))?;
                            let dash_keys = table
                                .keys()
                                .filter(|k| k.contains("-"))
                                .map(ToOwned::to_owned)
                                .collect::<Vec<_>>();
                            for key in dash_keys {
                                let value = table.remove(&key).unwrap();
                                table.insert(key.replace("-", "_"), value);
                            }
                            raw_config.try_into().map_err(|e| {
                                Error::with_chain(e, "Invalid config shape")
                            })
                        })
                })
        })
}

fn make_config() -> Result<(BlaccConfig, BlackConfig)> {
    let matches =
        App::new("Black Client")
            .version(crate_version!())
            .arg(Arg::with_name("verbose").short("-v").help("Set verbosity").long_help("Specify multiple times to increase verbosity. -v = info level logs, -vv = debug logs, -vvv = everything").multiple(true))
            .arg(Arg::with_name("quiet").short("q").help("Silence all logs").conflicts_with("quiet"))
            .arg(Arg::with_name("url").required(true).long("url").short("u").takes_value(true).help("URL of a running `blackd` server"))
            .arg(Arg::with_name("exclude").long("exclude").help("A regular expression that matches files and directories that should be excluded on recursive searches").long_help("An empty value means no paths are excluded. Exclusions are calculated first, inclusions later.").takes_value(true).default_value(DEFAULT_EXCLUDE))
            .arg(Arg::with_name("include").long("include").help("A regular expression that matches files and directories that should be included on recursive searches").long_help("An empty value means all files are included. Exclusions are calculated first, inclusions later.").takes_value(true).default_value(DEFAULT_INCLUDE))
            .arg(Arg::with_name("fast").long("fast").help("Skip sanity checks").conflicts_with("safe"))
            .arg(Arg::with_name("safe").long("safe").help("Run sanity checks after formatting").conflicts_with("fast"))
            .arg(Arg::with_name("skip-string-normalization").short("S").long("skip-string-normalization").help("Don't normalize string quotes or prefixes"))
            .arg(Arg::with_name("target-version").short("t").long("target-version").takes_value(true).use_delimiter(true).help("Python versions that should be supported by Black's output.").possible_values(&["py27", "py33", "py34", "py35", "py36", "py37", "py38"]))
            .arg(Arg::with_name("line-length").short("l").long("line-length").takes_value(true).help("How many characters per line to allow"))
            .arg(Arg::with_name("config-file").help("Path to TOML file containing black's configuration").long("config-file").takes_value(true))
            .arg(Arg::with_name("src").help("Input source(s) to be formatted").long_help("Input source(s) to be formatted. A single `-` means stdin.").takes_value(true).multiple(true).empty_values(false))
            .get_matches();
    let config_file = matches.value_of("config-file").map(|x| x.to_owned());
    let mut config = match config_file {
        Some(file) => read_config(file)?,
        None => Default::default(),
    };
    let srcs: Vec<String> = matches
        .values_of("src")
        .map(|x| x.map(|s| s.to_string()).collect())
        .unwrap_or_default();

    if let Some(exclude) = matches.value_of("exclude") {
        if exclude != DEFAULT_EXCLUDE || config.exclude.is_none() {
            config.exclude = Some(String::from(exclude));
        }
    }
    if let Some(include) = matches.value_of("include") {
        if include != DEFAULT_INCLUDE || config.include.is_none() {
            config.include = Some(String::from(include));
        }
    }
    if let Some(length) = matches.value_of("line-length") {
        config.line_length = Some(u64::from_str(length).unwrap());
    }
    if matches.is_present("fast") {
        config.fast = Some(true);
    }

    Ok((
        BlaccConfig {
            url: String::from(matches.value_of("url").unwrap()),
            quiet: matches.is_present("quiet"),
            verbosity: matches.occurrences_of("verbose") as usize,
            srcs,
        },
        config,
    ))
}

fn run() -> Result<()> {
    FD_LIMIT.store(get_fd_limit().unwrap_or(100), Ordering::Relaxed);
    let (config, black_config) =
        make_config().chain_err(|| "Unable to set up configuration")?;
    let logging = stderrlog::new()
        .module(module_path!())
        .quiet(config.quiet)
        .verbosity(config.verbosity + 1)
        .init()
        .chain_err(|| "Unable to set up logging");

    let exclude = RegexBuilder::new(
        black_config
            .exclude
            .as_ref()
            .chain_err(|| "no exclude regex set")?,
    )
    .ignore_whitespace(true)
    .build()
    .chain_err(|| "exclude is not a valid regex")?;
    let include = RegexBuilder::new(
        black_config
            .include
            .as_ref()
            .chain_err(|| "no include regex set")?,
    )
    .ignore_whitespace(true)
    .build()
    .chain_err(|| "include is not a valid regex")?;
    debug!(
        "blacc version {} config {:?}, {:?}",
        crate_version!(),
        &config,
        &black_config
    );
    let srcs = config.srcs;
    let url = Arc::new(config.url);
    let black_config = Arc::new(black_config);

    if srcs.len() == 0 {
        warn!("No paths given. Nothing to do 😴");
        return logging;
    }

    if srcs.len() == 1 && srcs[0] == "-" {
        let mut buf = Vec::new();
        let stdin = std::io::stdin();
        let result = stdin
            .lock()
            .read_to_end(&mut buf)
            .map(|_| buf)
            .map_err(|e| Error::with_chain(e, "Unable to read stdin"));
        let fut = format_bytes(result, url, black_config, None);
        tokio::run(fut);
        return logging;
    }

    let futs = srcs.into_iter().map(move |src| {
        let url = Arc::clone(&url);
        let black_config = Arc::clone(&black_config);
        let dirs = walk_dir(exclude.to_owned(), include.to_owned(), src);
        dirs.map(|files| {
            stream::iter_ok(files.into_iter().map(move |src| {
                let fname = Arc::new(src);
                let url = Arc::clone(&url);
                let black_config = Arc::clone(&black_config);

                fs::read(String::clone(fname.borrow()))
                    .chain_err(|| "error opening file")
                    .then(move |contents| {
                        format_bytes(contents, url, black_config, Some(fname))
                    })
            }))
            .buffered(FD_LIMIT.load(Ordering::Relaxed) / 2)
            .collect()
        })
    });

    tokio::run(join_all(futs.map(|x| x.flatten())).map(|_| ()));

    logging
}

fn format_bytes(
    contents: Result<Vec<u8>>,
    url: Arc<String>,
    config: Arc<BlackConfig>,
    fname: Option<Arc<String>>,
) -> impl Future<Item = (), Error = ()> {
    let fname_for_display = match &fname {
        Some(fname) => Arc::clone(fname),
        None => Arc::new("<stdin>".to_string()),
    };
    future::result(contents)
        .and_then(move |contents| send_req(&url, &config, contents))
        .and_then(move |resp| handle_response(resp, fname))
        .then(move |result| {
            match result {
                Ok(action) => info!("{}: {}", fname_for_display, action),
                Err(err) => {
                    error!("{}: {}", fname_for_display, err.display_chain().to_string())
                }
            }
            Ok(())
        })
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
