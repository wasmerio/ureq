use body::{send_body, Payload, SizedReader};
use std::io::{Result as IoResult, Write};
use stream::{connect_http, connect_https, connect_test, Stream};
use url::Url;
//

use pool::DEFAULT_HOST;

/// It's a "unit of work". Maybe a bad name for it?
///
/// *Internal API*
#[derive(Debug)]
pub struct Unit {
    pub agent: Arc<Mutex<Option<AgentState>>>,
    pub url: Url,
    pub is_chunked: bool,
    pub is_head: bool,
    pub hostname: String,
    pub query_string: String,
    pub headers: Vec<Header>,
    pub timeout_connect: u64,
    pub timeout_read: u64,
    pub timeout_write: u64,
}

impl Unit {
    //

    fn new(req: &Request, url: &Url, body: &SizedReader) -> Self {
        //

        let is_chunked = req.header("transfer-encoding")
            // if the user has set an encoding header, obey that.
            .map(|enc| enc.len() > 0)
            // otherwise, no chunking.
            .unwrap_or(false);

        let is_secure = url.scheme().eq_ignore_ascii_case("https");

        let is_head = req.method.eq_ignore_ascii_case("head");

        let hostname = url.host_str().unwrap_or(DEFAULT_HOST).to_string();

        let query_string = combine_query(&url, &req.query);

        let cookie_headers: Vec<_> = {
            let mut state = req.agent.lock().unwrap();
            match state.as_ref().map(|state| &state.jar) {
                None => vec![],
                Some(jar) => match_cookies(jar, &hostname, url.path(), is_secure),
            }
        };
        let extra_headers = {
            let mut extra = vec![];

            // chunking and Content-Length headers are mutually exclusive
            // also don't write this if the user has set it themselves
            if !is_chunked && !req.has("content-length") {
                if let Some(size) = body.size {
                    extra.push(
                        format!("Content-Length: {}\r\n", size)
                            .parse::<Header>()
                            .unwrap(),
                    );
                }
            }
            extra
        };
        let headers: Vec<_> = req
            .headers
            .iter()
            .chain(cookie_headers.iter())
            .chain(extra_headers.iter())
            .cloned()
            .collect();

        Unit {
            agent: Arc::clone(&req.agent),
            url: url.clone(),
            is_chunked,
            is_head,
            hostname,
            query_string,
            headers,
            timeout_connect: req.timeout_connect,
            timeout_read: req.timeout_read,
            timeout_write: req.timeout_write,
        }
    }

    #[cfg(test)]
    pub fn header<'a>(&self, name: &'a str) -> Option<&str> {
        get_header(&self.headers, name)
    }
    #[cfg(test)]
    pub fn has<'a>(&self, name: &'a str) -> bool {
        has_header(&self.headers, name)
    }
    #[cfg(test)]
    pub fn all<'a>(&self, name: &'a str) -> Vec<&str> {
        get_all_headers(&self.headers, name)
    }
}

/// Perform a connection. Used recursively for redirects.
pub fn connect(
    mut unit: Unit,
    method: &str,
    use_pooled: bool,
    redirects: u32,
    body: SizedReader,
) -> Result<Response, Error> {
    //

    // open socket
    let (mut stream, is_recycled) = connect_socket(&unit, use_pooled)?;

    let send_result = send_prelude(&unit, method, &mut stream);

    if send_result.is_err() {
        if is_recycled {
            // we try open a new connection, this time there will be
            // no connection in the pool. don't use it.
            return connect(unit, method, false, redirects, body);
        } else {
            // not a pooled connection, propagate the error.
            return Err(send_result.unwrap_err().into());
        }
    }

    // start reading the response to process cookies and redirects.
    let mut resp = Response::from_read(&mut stream);

    // squirrel away cookies
    save_cookies(&unit, &resp);

    // handle redirects
    if resp.redirect() {
        if redirects == 0 {
            return Err(Error::TooManyRedirects);
        }

        // the location header
        let location = resp.header("location");
        if let Some(location) = location {
            // join location header to current url in case it it relative
            let new_url = unit
                .url
                .join(location)
                .map_err(|_| Error::BadUrl(format!("Bad redirection: {}", location)))?;

            // change this for every redirect since it is used when connection pooling.
            unit.url = new_url;

            // perform the redirect differently depending on 3xx code.
            return match resp.status() {
                301 | 302 | 303 => {
                    send_body(body, unit.is_chunked, &mut stream)?;
                    let empty = Payload::Empty.into_read();
                    connect(unit, "GET", use_pooled, redirects - 1, empty)
                }
                307 | 308 | _ => connect(unit, method, use_pooled, redirects - 1, body),
            };
        }
    }

    // send the body (which can be empty now depending on redirects)
    send_body(body, unit.is_chunked, &mut stream)?;

    // since it is not a redirect, give away the incoming stream to the response object
    response::set_stream(&mut resp, Some(unit), stream);

    // release the response
    Ok(resp)
}

// TODO check so cookies can't be set for tld:s
fn match_cookies<'a>(jar: &'a CookieJar, domain: &str, path: &str, is_secure: bool) -> Vec<Header> {
    jar.iter()
        .filter(|c| {
            // if there is a domain, it must be matched.
            // if there is no domain, then ignore cookie
            let domain_ok = c
                .domain()
                .map(|cdom| domain.contains(cdom))
                .unwrap_or(false);
            // a path must match the beginning of request path.
            // no cookie path, we say is ok. is it?!
            let path_ok = c
                .path()
                .map(|cpath| path.find(cpath).map(|pos| pos == 0).unwrap_or(false))
                .unwrap_or(true);
            // either the cookie isnt secure, or we're not doing a secure request.
            let secure_ok = !c.secure() || is_secure;

            domain_ok && path_ok && secure_ok
        })
        .map(|c| {
            let name = c.name().to_string();
            let value = c.value().to_string();
            let nameval = Cookie::new(name, value).encoded().to_string();
            let head = format!("Cookie: {}", nameval);
            head.parse::<Header>().ok()
        })
        .filter(|o| o.is_some())
        .map(|o| o.unwrap())
        .collect()
}

/// Combine the query of the url and the query options set on the request object.
fn combine_query(url: &Url, query: &QString) -> String {
    match (url.query(), query.len() > 0) {
        (Some(urlq), true) => format!("?{}&{}", urlq, query),
        (Some(urlq), false) => format!("?{}", urlq),
        (None, true) => format!("?{}", query),
        (None, false) => "".to_string(),
    }
}

/// Connect the socket, either by using the pool or grab a new one.
fn connect_socket(unit: &Unit, use_pooled: bool) -> Result<(Stream, bool), Error> {
    if use_pooled {
        let state = &mut unit.agent.lock().unwrap();
        if let Some(agent) = state.as_mut() {
            if let Some(stream) = agent.pool.try_get_connection(&unit.url) {
                return Ok((stream, true));
            }
        }
    }
    let stream = match unit.url.scheme() {
        "http" => connect_http(&unit),
        "https" => connect_https(&unit),
        "test" => connect_test(&unit),
        _ => Err(Error::UnknownScheme(unit.url.scheme().to_string())),
    };
    Ok((stream?, false))
}

/// Send request line + headers (all up until the body).
fn send_prelude(unit: &Unit, method: &str, stream: &mut Stream) -> IoResult<()> {
    //

    // build into a buffer and send in one go.
    let mut prelude: Vec<u8> = vec![];

    // request line
    write!(
        prelude,
        "{} {}{} HTTP/1.1\r\n",
        method,
        unit.url.path(),
        &unit.query_string
    )?;

    // host header if not set by user.
    if !has_header(&unit.headers, "host") {
        write!(prelude, "Host: {}\r\n", unit.url.host().unwrap())?;
    }

    // other headers
    for header in &unit.headers {
        write!(prelude, "{}: {}\r\n", header.name(), header.value())?;
    }

    // finish
    write!(prelude, "\r\n")?;

    // write all to the wire
    stream.write_all(&mut prelude[..])?;

    Ok(())
}

/// Investigate a response for "Set-Cookie" headers.
fn save_cookies(unit: &Unit, resp: &Response) {
    //

    let cookies = resp.all("set-cookie");
    if cookies.is_empty() {
        return;
    }

    // only lock if we know there is something to process
    let state = &mut unit.agent.lock().unwrap();
    if let Some(add_jar) = state.as_mut().map(|state| &mut state.jar) {
        for raw_cookie in cookies.iter() {
            let to_parse = if raw_cookie.to_lowercase().contains("domain=") {
                raw_cookie.to_string()
            } else {
                format!("{}; Domain={}", raw_cookie, &unit.hostname)
            };
            match Cookie::parse_encoded(&to_parse[..]) {
                Err(_) => (), // ignore unparseable cookies
                Ok(mut cookie) => {
                    let cookie = cookie.into_owned();
                    add_jar.add(cookie)
                }
            }
        }
    }
}
