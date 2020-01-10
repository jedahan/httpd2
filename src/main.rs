use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use std::time::SystemTime;

use hyper::service::service_fn;
use hyper::{Body, Method, Request, Response, StatusCode};

use rustls::{NoClientAuth, ProtocolVersion, ServerConfig};

use tokio::fs;
use tokio::stream::StreamExt;
use tokio_rustls::TlsAcceptor;
use tokio_util::codec::{self, Decoder};

/// Error union type for the server.
#[derive(Debug)]
enum ServeError {
    /// Errors coming from within Hyper.
    Hyper(hyper::Error),
    /// I/O-related errors.
    Io(io::Error),
    /// Errors in the Nix syscall interface.
    Nix(nix::Error),
    /// Errors in the TLS subsystem.
    Tls(rustls::TLSError),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ServeError::Hyper(e) => write!(f, "{}", e),
            ServeError::Io(e) => write!(f, "{}", e),
            ServeError::Nix(e) => write!(f, "{}", e),
            ServeError::Tls(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServeError::Hyper(e) => Some(e),
            ServeError::Io(e) => Some(e),
            ServeError::Nix(e) => Some(e),
            ServeError::Tls(e) => Some(e),
        }
    }
}

impl From<rustls::TLSError> for ServeError {
    fn from(x: rustls::TLSError) -> Self {
        ServeError::Tls(x)
    }
}

impl From<hyper::Error> for ServeError {
    fn from(x: hyper::Error) -> Self {
        ServeError::Hyper(x)
    }
}

impl From<nix::Error> for ServeError {
    fn from(x: nix::Error) -> Self {
        ServeError::Nix(x)
    }
}

impl From<io::Error> for ServeError {
    fn from(x: io::Error) -> Self {
        ServeError::Io(x)
    }
}

/// Possible successful results from the `picky_open` family of operations.
enum FileOrDir {
    /// We found a regular file, with permissions set such that we're willing to
    /// admit it exists.
    File {
        /// An async handle to the file, open for read.
        file: fs::File,
        /// The file's inferred content type.
        content_type: &'static str,
        /// Length of the file in bytes.
        len: u64,
        /// Modification timestamp.
        modified: SystemTime,
    },
    /// We found a directory.
    Dir,
}

/// Accesses a path for file serving, if it meets certain narrow criteria.
///
/// This operation is critical to the correctness of the server. It is careful
/// in several respects:
///
/// 1. To avoid TOCTOU issues, it opens files first and checks their metadata
///    second.
///
/// 2. Only files that are user/group/world readable are acknowledged to exist.
///
/// 3. Files that are world-X but not user-X are rejected, for reasons inherited
///    from publicfile that I don't quite recall.
///
/// If the path turns out to be a directory, returns `FileOrDir::Dir` only if it
/// meets all the above criteria.
async fn picky_open(log: &slog::Logger, path: &Path) -> Result<FileOrDir, io::Error> {
    slog::debug!(log, "picky_open({:?})", path);

    use std::os::unix::fs::PermissionsExt;
    let file = fs::File::open(path).await
        .map_err(|e| {
            slog::debug!(log, "can't open: {}", e);
            e
        })?;
    let meta = file.metadata().await?;
    let mode = meta.permissions().mode();

    if mode & 0o444 != 0o444 || mode & 0o101 == 0o001 {
        slog::debug!(log, "mode {:#o} is not OK", mode);
        Err(io::Error::new(io::ErrorKind::NotFound, "perms"))
    } else if meta.is_file() {
        slog::debug!(log, "opened");
        Ok(FileOrDir::File {
            file,
            content_type: map_content_type(path),
            len: meta.len(),
            modified: meta.modified().unwrap(),
        })
    } else if meta.is_dir() {
        slog::debug!(log, "found dir");
        Ok(FileOrDir::Dir)
    } else {
        slog::debug!(log, "neither file nor dir");
        Err(io::Error::new(io::ErrorKind::NotFound, "type"))
    }
}

/// Extends `picky_open` with directory redirect handling.
///
/// If `path` turns out to be a directory, this routine will retry the
/// `picky_open` to search for an `index.html` file within that directory. If
/// the `index.html` has the appropriate permissions and is a regular file, the
/// open operation succeeds, returning its contents.
async fn picky_open_with_redirect(
    log: &slog::Logger,
    path: &mut String,
) -> Result<FileOrDir, io::Error> {
    match picky_open(log, Path::new(path)).await? {
        FileOrDir::Dir => {
            slog::debug!(log, "--> index.html");
            path.push_str("/index.html");
            picky_open(log, Path::new(path)).await
        }
        r => Ok(r),
    }
}

/// Extends `picky_open_with_redirect` with selection of precompressed
/// alternate files.
///
/// When `picky_open_with_redirect` finds a readable regular file at `path`,
/// this routine will retry to search for a compressed version of the file with
/// the same name and the `.gz` extension appended. If the compressed version
/// exists, passes `picky_open`'s criteria, *and* has a last-modified date at
/// least as recent as the original file, then it is substituted.
///
/// Importantly, the content-type judgment for the *original*, non-compressed
/// file, is preserved.
///
/// Returns the normal `FileOrDir` result, plus an optional `Content-Encoding`
/// value if an alternate encoding was selected.
async fn picky_open_with_redirect_and_gzip(
    log: &slog::Logger,
    path: &mut String,
) -> Result<(FileOrDir, Option<&'static str>), io::Error> {
    match picky_open_with_redirect(log, path).await? {
        FileOrDir::Dir => Ok((FileOrDir::Dir, None)),
        FileOrDir::File {
            file,
            len,
            content_type,
            modified,
        } => {
            slog::debug!(log, "checking for precompressed alternate");
            path.push_str(".gz");
            match picky_open(log, Path::new(path)).await {
                Ok(FileOrDir::File {
                    file,
                    len,
                    modified: cmod,
                    ..
                }) if cmod >= modified => {
                    slog::debug!(log, "serving gzip");
                    Ok((
                        FileOrDir::File {
                            file,
                            len,
                            content_type,
                            modified,
                        },
                        Some("gzip"),
                    ))
                },
                _ => {
                    slog::debug!(log, "serving uncompressed");
                    Ok((
                        FileOrDir::File {
                            file,
                            len,
                            content_type,
                            modified,
                        },
                        None,
                    ))
                }
            }
        }
    }
}

/// Guesses the `Content-Type` of a file based on its path.
///
/// Currently, this is hardcoded based on file extensions, like we're Windows.
fn map_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str) {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        _ => "text/plain",
    }
}

struct Sanitizer<I> {
    inner: I,
    state: SanitizerState,
}

impl<I> From<I> for Sanitizer<I> {
    fn from(inner: I) -> Self {
        Self { inner, state: SanitizerState::EmitDot }
    }
}

#[derive(Copy, Clone, Debug)]
enum SanitizerState {
    EmitDot,
    EmitSlash,
    Normal,
    Slash,
}

impl<I: Iterator<Item = char>> Iterator for Sanitizer<I> {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            SanitizerState::EmitDot => {
                self.state = SanitizerState::EmitSlash;
                return Some('.')
            }
            SanitizerState::EmitSlash => {
                self.state = SanitizerState::Slash;
                return Some('/')
            }
            _ => (),
        }

        loop {
            match (self.state, self.inner.next()?) {
                (_, '\0') => {
                    self.state = SanitizerState::Normal;
                    break Some('_')
                }
                (SanitizerState::Normal, '/') => {
                    self.state = SanitizerState::Slash;
                    break Some('/')
                }
                (SanitizerState::Slash, '/') => continue,
                (SanitizerState::Slash, '.') => {
                    self.state = SanitizerState::Normal;
                    break Some(':')
                }
                (_, c) => {
                    self.state = SanitizerState::Normal;
                    break Some(c)
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // We alter the inner size-hint because it's possible that we discard
        // all characters. The max length is extended by the initial dot-slash.
        (0, self.inner.size_hint().1.map(|x| x + 2))
    }
}


fn sanitize_path(path: &str) -> String {
    Sanitizer::from(PercentDecoder::from(path.chars())).collect()
}

struct PercentDecoder<I> {
    inner: I,
    state: PercentState,
}

impl<I> From<I> for PercentDecoder<I> {
    fn from(inner: I) -> Self {
        Self {
            inner,
            state: PercentState::Normal,
        }
    }
}

enum PercentState {
    /// Haven't seen a percent escape recently.
    Normal,
    /// A percent escape was found to be invalid on its final character. We have
    /// yielded the original '%' and need to yield these additional characters
    /// in sequence before touching `inner`.
    Unspool2(char, char),
    /// A percent escape was found to be invalid. We have yielded some portion
    /// of it literally, and still need to yield this char before touching
    /// `inner`.
    Unspool(char),
}

impl<I: Iterator<Item = char>> Iterator for PercentDecoder<I> {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        fn hexit(c: char) -> Option<u8> {
            match c {
                '0'..='9' => Some(c as u8 - '0' as u8),
                'A'..='F' => Some(c as u8 - 'A' as u8 + 10),
                'a'..='f' => Some(c as u8 - 'a' as u8 + 10),
                _ => None,
            }
        }

        match self.state {
            PercentState::Normal => {
                match self.inner.next()? {
                    '%' => {
                        if let Some(x) = self.inner.next() {
                            if let Some(y) = self.inner.next() {
                                if let (Some(x), Some(y)) = (hexit(x), hexit(y)) {
                                    return Some((x << 4 | y) as char)
                                } else {
                                    self.state = PercentState::Unspool2(x, y);
                                }
                            } else {
                                self.state = PercentState::Unspool(x);
                            }
                        }
                        Some('%')
                    }
                    c => Some(c)
                }
            }
            PercentState::Unspool2(x, y) => {
                self.state = PercentState::Unspool(y);
                Some(x)
            }
            PercentState::Unspool(y) => {
                self.state = PercentState::Normal;
                Some(y)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (min, max) = self.inner.size_hint();
        (min / 3, max)
    }
}

/// Attempts to serve a file in response to `req`.
async fn serve_files(log: slog::Logger, req: Request<Body>) -> Result<Response<Body>, ServeError> {
    let mut response = Response::new(Body::empty());

    // Scan the request headers to see if gzip compressed responses are OK.
    let mut accept_gzip = false;
    for list in req.headers().get_all(hyper::header::ACCEPT_ENCODING).iter() {
        if let Ok(list) = list.to_str() {
            if list.split(",").any(|item| item.trim() == "gzip") {
                accept_gzip = true;
                break;
            }
        }
    }

    // Process GET requests.
    let method = req.method();
    match (method, req.uri().path()) {
        (&Method::GET, path) | (&Method::HEAD, path) => {
            // Sanitize the path using a derivative of publicfile's algorithm.
            // It appears that Hyper blocks non-ASCII characters.
            // Allocate enough room for a path that doesn't require
            // sanitization, plus the initial dot-slash.
            slog::info!(log, "{} {}", method, path);
            let mut sanitized = sanitize_path(path);

            // Select content encoding.
            let open_result = if accept_gzip {
                picky_open_with_redirect_and_gzip(&log, &mut sanitized).await
            } else {
                picky_open_with_redirect(&log, &mut sanitized)
                    .await
                    .map(|f| (f, None))
            };

            match open_result {
                Ok((
                    FileOrDir::File {
                        file,
                        content_type,
                        len,
                        modified,
                    },
                    enc,
                )) => {
                    use hyper::header::HeaderValue;

                    response
                        .headers_mut()
                        .insert(hyper::header::CONTENT_LENGTH, len.into());
                    response.headers_mut().insert(
                        hyper::header::CONTENT_TYPE,
                        HeaderValue::from_static(content_type),
                    );
                    response.headers_mut().insert(
                        hyper::header::LAST_MODIFIED,
                        HeaderValue::from_str(&httpdate::fmt_http_date(
                            modified,
                        ))
                        .unwrap(),
                    );
                    if let Some(enc) = enc {
                        response.headers_mut().insert(
                            hyper::header::CONTENT_ENCODING,
                            HeaderValue::from_static(enc),
                        );
                    }

                    if method == Method::GET {
                        slog::info!(log, "OK: len={} encoding={:?}", len, enc);
                        *response.body_mut() = Body::wrap_stream(
                            codec::BytesCodec::new()
                                .framed(file)
                                .map(|b| b.map(bytes::BytesMut::freeze)),
                        );
                    }
                }
                // To avoid disclosing information, we signal any other case
                // as 404. Cases covered here include:
                // - Actual file not found.
                // - Permissions did not permit file to be served.
                // - One level of directory redirect followed, but still
                //   found a directory.
                Ok(_) => {
                    slog::info!(log, "failed: would serve directory");
                    *response.status_mut() = StatusCode::NOT_FOUND;
                }
                Err(e) => {
                    slog::info!(log, "failed: {}", e);
                    *response.status_mut() = StatusCode::NOT_FOUND;
                }
            }
        }
        _ => {
            // Any other request method falls here.
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    }

    Ok(response)
}

const DEFAULT_IP: std::net::Ipv6Addr = std::net::Ipv6Addr::UNSPECIFIED;
const DEFAULT_PORT: u16 = 8000;

struct Args {
    root: std::path::PathBuf,
    key_path: std::path::PathBuf,
    cert_path: std::path::PathBuf,
    should_chroot: bool,
    addr: SocketAddr,
    uid: Option<nix::unistd::Uid>,
    gid: Option<nix::unistd::Gid>,
}

fn get_args() -> Result<Args, clap::Error> {
    let matches = clap::App::new("httpd2")
        .arg(
            clap::Arg::with_name("chroot")
                .short("c")
                .long("chroot")
                .help(
                    "Specifies that the server should chroot into DIR. You\n\
                     basically always want this, unless you're running the\n\
                     server as an unprivileged user.",
                ),
        )
        .arg(
            clap::Arg::with_name("addr")
                .short("A")
                .long("addr")
                .takes_value(true)
                .value_name("ADDR:PORT")
                .validator(is_sockaddr)
                .help("Address and port to bind."),
        )
        .arg(
            clap::Arg::with_name("uid")
                .short("U")
                .long("uid")
                .takes_value(true)
                .value_name("UID")
                .validator(is_uid)
                .help("User to switch to via setuid before serving."),
        )
        .arg(
            clap::Arg::with_name("gid")
                .short("G")
                .long("gid")
                .takes_value(true)
                .value_name("GID")
                .validator(is_gid)
                .help("Group to switch to via setgid before serving."),
        )
        .arg(
            clap::Arg::with_name("key_path")
                .short("k")
                .long("key-path")
                .takes_value(true)
                .value_name("PATH")
                .default_value("localhost.key")
                .help("Location of TLS private key."),
        )
        .arg(
            clap::Arg::with_name("cert_path")
                .short("r")
                .long("cert-path")
                .takes_value(true)
                .value_name("PATH")
                .default_value("localhost.crt")
                .help("Location of TLS certificate."),
        )
        .arg(
            clap::Arg::with_name("DIR")
                .help("Path to serve")
                .required(true)
                .index(1),
        )
        .get_matches();

    fn is_uid(val: String) -> Result<(), String> {
        val.parse::<libc::uid_t>()
            .map(|_| ())
            .map_err(|_| "can't parse as UID".to_string())
    }

    fn is_gid(val: String) -> Result<(), String> {
        val.parse::<libc::uid_t>()
            .map(|_| ())
            .map_err(|_| "can't parse as GID".to_string())
    }

    fn is_sockaddr(val: String) -> Result<(), String> {
        val.parse::<SocketAddr>()
            .map(|_| ())
            .map_err(|_| "can't parse as addr:port".to_string())
    }

    use clap::value_t;

    let root = matches.value_of("DIR").unwrap();
    let key_path = matches.value_of("key_path").unwrap();
    let cert_path = matches.value_of("cert_path").unwrap();
    let should_chroot = value_t!(matches, "chroot", bool).unwrap_or(false);
    let addr = value_t!(matches, "addr", SocketAddr)
        .unwrap_or(SocketAddr::from((DEFAULT_IP, DEFAULT_PORT)));
    println!("{:?}", addr);

    let uid = matches.value_of("uid").map(|uid| {
        nix::unistd::Uid::from_raw(uid.parse::<libc::uid_t>().unwrap())
    });
    let gid = matches.value_of("gid").map(|gid| {
        nix::unistd::Gid::from_raw(gid.parse::<libc::gid_t>().unwrap())
    });

    Ok(Args {
        root: std::path::PathBuf::from(root),
        key_path: std::path::PathBuf::from(key_path),
        cert_path: std::path::PathBuf::from(cert_path),
        should_chroot,
        addr,
        uid,
        gid,
    })
}

fn load_key_and_cert(
    key_path: &Path,
    cert_path: &Path,
) -> io::Result<(rustls::PrivateKey, Vec<rustls::Certificate>)> {
    let key = rustls::internal::pemfile::pkcs8_private_keys(
        &mut io::BufReader::new(std::fs::File::open(key_path)?),
    )
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::Other,
            "can't load private key (bad file?)",
        )
    })?
    .pop()
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "no keys found in private key file",
        )
    })?;
    let cert_chain = rustls::internal::pemfile::certs(&mut io::BufReader::new(
        std::fs::File::open(cert_path)?,
    ))
    .map_err(|_| {
        io::Error::new(io::ErrorKind::Other, "can't load certificate")
    })?;
    Ok((key, cert_chain))
}

fn drop_privs(args: &Args) -> Result<(), ServeError> {
    std::env::set_current_dir(&args.root)?;
    if args.should_chroot {
        nix::unistd::chroot(&args.root)?;
    }
    if let Some(gid) = args.gid {
        nix::unistd::setgid(gid)?;
        nix::unistd::setgroups(&[gid])?;
    }
    if let Some(uid) = args.uid {
        nix::unistd::setuid(uid)?;
    }
    Ok(())
}

async fn start(log: slog::Logger) -> Result<(), ServeError> {
    // Go ahead and parse arguments before dropping privileges, since they
    // control whether we drop privileges, among other things.
    let args = match get_args() {
        Ok(args) => args,
        Err(e) => e.exit(),
    };

    // Things that need to get done while root:
    // - Binding to privileged ports.
    // - Reading SSL private key.
    // - Chrooting.

    let (key, cert_chain) = load_key_and_cert(
        &args.key_path,
        &args.cert_path,
    )?;

    let mut listener = tokio::net::TcpListener::bind(&args.addr).await?;

    // Dropping privileges here...
    drop_privs(&args)?;

    let tls_acceptor = {
        let mut config = ServerConfig::new(NoClientAuth::new());
        config.set_single_cert(cert_chain, key)?;
        config.versions =
            vec![ProtocolVersion::TLSv1_3, ProtocolVersion::TLSv1_2];
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        TlsAcceptor::from(Arc::new(config))
    };
    let http = hyper::server::conn::Http::new();

    let mut incoming = listener.incoming();
    let connection_counter = AtomicU64::new(0);
    while let Some(stream) = incoming.next().await {
        if let Ok(socket) = stream {
            let log = log.new(slog::o!(
                "peer" => socket.peer_addr().map(|sa| sa.to_string()).unwrap_or_else(|_| "UNKNOWN".to_string()),
                "cid" => connection_counter.fetch_add(1, Ordering::Relaxed),
            ));
            let tls_acceptor = tls_acceptor.clone();
            let http = http.clone();
            tokio::spawn(async move {
                match tls_acceptor.accept(socket).await {
                Ok(stream) => {
                    use rustls::Session;

                    let session = stream.get_ref().1;
                    slog::debug!(log, "ALPN result: {:?}", std::str::from_utf8(session.get_alpn_protocol().unwrap_or(b"NONE")).unwrap_or("BOGUS").to_string());
                    let request_counter = AtomicU64::new(0);
                    let r = http
                        .serve_connection(stream, service_fn(|x| {
                            let log = log.new(slog::o!(
                                "rid" => request_counter.fetch_add(1, Ordering::Relaxed),
                            ));
                            serve_files(log, x)
                        }))
                        .await;
                    if let Err(e) = r {
                        slog::debug!(log, "error in connection: {}", e);
                    }
                    slog::info!(log, "connection closed");
                }
                Err(e) => {
                    slog::warn!(log, "error in TLS handshake: {}", e);
                }
            }
            });
        } else {
            slog::warn!(log, "error accepting");
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    use slog::Drain;

    let decorator = slog_term::PlainDecorator::new(std::io::stderr());
    let drain = slog_term::FullFormat::new(decorator).use_original_order().build().fuse();
    let drain = slog_async::Async::new(drain).chan_size(1024).build().fuse();
    let log = slog::Logger::root(drain, slog::o!());

    start(log).await.expect("server failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_path(""), "./");
        assert_eq!(sanitize_path("///"), "./");
        assert_eq!(sanitize_path("."), "./:");
        assert_eq!(sanitize_path("/."), "./:");
        assert_eq!(sanitize_path(".."), "./:.");
        assert_eq!(sanitize_path("\0"), "./_");
        assert_eq!(sanitize_path("/\0"), "./_");

        assert_eq!(sanitize_path("//.././doc.pdf\0/"), "./:./:/doc.pdf_/");
    }

    #[test]
    fn percent_decode() {
        assert_eq!(sanitize_path(""), "./");
        assert_eq!(sanitize_path("%"), "./%");
        assert_eq!(sanitize_path("%4"), "./%4");
        assert_eq!(sanitize_path("%41"), "./A");
        assert_eq!(sanitize_path("%4a"), "./J");
        assert_eq!(sanitize_path("%4A"), "./J");
        assert_eq!(sanitize_path("%4g"), "./%4g");
        assert_eq!(sanitize_path("%2525"), "./%25");
    }

    #[test]
    fn percent_and_sanitize() {
        assert_eq!(sanitize_path("%2f"), "./");
        assert_eq!(sanitize_path("%2f%2F"), "./");
        assert_eq!(sanitize_path("%2f%2e%2e"), "./:.");
        assert_eq!(sanitize_path("%2f%2e%2e%00"), "./:._");
    }
}
