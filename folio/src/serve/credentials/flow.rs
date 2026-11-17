//! A bottle's request to a credential's host: read, checked against its rules, its credential put in, sent on.

use super::*;

/// A request inside a credential's host's TLS, made ready for the host.
pub(super) struct Request {
    method: String,
    /// What's sent on: its head, and its body if a rewrite read it.
    upstream: Vec<u8>,
    path: String,
    injected: bool,
}

/// Is the far end of `stream`, a flow bottle-cli gave here unwrapped, for `injection`'s host.
pub fn serve(bottle: &Bottle, injection: &Injection, header: &Header, stream: UnixStream) -> Flow {
    let name = header.name.clone().unwrap_or_default();
    let mut served = Flow {
        to: "-".into(),
        outcome: "ok".into(),
        status: None,
        up: 0,
        down: 0,
        injected: false,
    };
    let _ = stream.set_read_timeout(Some(http::HEADER_TIMEOUT));
    let Ok(read) = stream.try_clone() else {
        served.outcome = "error".into();
        return served;
    };
    let mut reader = BufReader::new(read);
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => {
            served.outcome = "error".into();
            return served;
        }
    };
    let result = (|| -> Result<(), Refused> {
        let request = inside(
            bottle,
            &name,
            header.port,
            injection,
            &mut reader,
            &mut writer,
        )?;
        served.injected = request.injected;
        let addresses = allowed(&name, header.port)?;
        let (ip, conn, tcp) = open(&name, header.port, &addresses)?;
        served.to = ip.to_string();
        let _ = stream.set_read_timeout(None);
        let mut peek = Vec::new();
        // What the bottle sent past the head, already read: the start of the rest.
        let buffered = reader.buffer().to_vec();
        let (up, down) = relay_tls(&stream, conn, tcp, &request.upstream, &buffered, &mut peek)
            .map_err(|_| Refused {
                status: None,
                reason: String::new(),
                outcome: "error",
                body: None,
                content_type: "",
            })?;
        served.up = up;
        served.down = down;
        served.status = status_of(&peek);
        // Each route, and whether it took the credential: what a feature's allow list is made from.
        log!(
            "bottle.egress",
            "{} {} https://{name}{} {} {}",
            bottle.name,
            request.method,
            request.path,
            if request.injected {
                "credential"
            } else {
                "passed"
            },
            served.status.as_deref().unwrap_or("-")
        );
        Ok(())
    })();
    if let Err(refused) = result {
        served.outcome = refused.outcome.into();
        // Answered in plain http: bottle-cli wraps it for the bottle, which spoke TLS.
        http::respond_refused(&mut writer, &refused);
    }
    served
}

/// The request the bottle sent inside TLS terminated for host `name`, made ready for the host: its
/// head, and its body too when a rewrite reads it for the stand-in.
pub(super) fn inside(
    bottle: &Bottle,
    name: &str,
    port: u16,
    injection: &Injection,
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
) -> Result<Request, Refused> {
    let head = http::read_head(reader)?;
    let (scheme, authority, path, query) = split_target(&head.target);
    if head.method == "CONNECT"
        || scheme.is_some_and(|s| s != "https")
        || authority
            .as_deref()
            .is_some_and(|a| host_of(a) != name.to_lowercase())
    {
        return Err(Refused::bad_request());
    }
    if ECHOING_METHODS.contains(&head.method.to_uppercase().as_str()) {
        // The response would carry the credential back into the bottle.
        return Err(Refused::new(405, "Method Not Allowed", "denied"));
    }
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    let logged = if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    };
    let Some(route) = crate::routes::canonical(&path) else {
        return Err(Refused::bad_request());
    };
    let headers = head.headers();
    let log_request = |outcome: &str| {
        log!(
            "bottle.egress",
            "{} {} https://{name}{logged} {outcome} 403",
            bottle.name,
            head.method
        );
    };
    if injection.forbids(&head.method, &route) {
        log_request("forbidden");
        return Err(route_refused(
            &headers,
            &format!(
                "bottle refused {} {path} on {name}: the {}'s feature forbids this route, with the credential or without.",
                head.method,
                injection.named()
            ),
            "forbidden",
        ));
    }
    let values_of = |wanted: &str| -> Vec<String> {
        head.lines
            .iter()
            .filter(|l| http::name_of(l) == wanted.to_lowercase())
            .map(|l| l.split_once(':').map_or("", |(_, v)| v).to_string())
            .collect()
    };
    // The credential, if the bottle presented the stand-in under this name.
    let value = values_of(&injection.header)
        .iter()
        .find_map(|v| injection.replacing(v));
    // And where else this route's tool presents it.
    let rewrites: Vec<&Rewrite> = if injection.credential.is_empty() {
        vec![]
    } else {
        injection
            .rewrites
            .iter()
            .filter(|r| r.route.matches(&head.method, &route))
            .collect()
    };
    let mut rewritten: BTreeMap<String, String> = BTreeMap::new();
    for r in rewrites.iter().filter(|r| !r.header.is_empty()) {
        if values_of(&r.header)
            .iter()
            .any(|v| presents(r, v, &injection.standin))
        {
            rewritten.insert(
                r.header.to_lowercase(),
                format!("{}: {}", r.header, encoded(r, &injection.credential)),
            );
        }
    }
    let (mut body, mut in_body) = (None, false);
    let fields: Vec<&Rewrite> = rewrites
        .iter()
        .copied()
        .filter(|r| !r.json.is_empty())
        .collect();
    if !fields.is_empty() {
        let read = request_body(reader, writer, &headers)?;
        let (with, did) = json_with_credential(&read, &fields, injection);
        body = Some(with);
        in_body = did;
    }
    if (value.is_some() || !rewritten.is_empty() || in_body)
        && !injection.allows(&head.method, &route)
    {
        log_request("not-allowed");
        return Err(route_refused(
            &headers,
            &format!(
                "bottle refused {} {path} on {name}: the {} goes only on the routes its feature allows, and this isn't one of them.",
                head.method,
                injection.named()
            ),
            "not-allowed",
        ));
    }
    // The Host header goes with the destination, and the routing headers go altogether, so a
    // request carrying a credential can't ask one server to answer as another, or for something
    // else. Nothing else differs between the requests that carry it and those that don't.
    let mut dropped: Vec<String> = ["host"]
        .iter()
        .chain(ROUTING_HEADERS.iter())
        .map(|s| s.to_string())
        .collect();
    dropped.extend(HOP_HEADERS.iter().map(|s| s.to_string()));
    if value.is_some() {
        dropped.push(injection.header.to_lowercase());
    }
    dropped.extend(rewritten.keys().cloned());
    if body.is_some() {
        // Read here, and sent whole.
        dropped.extend(["content-length", "transfer-encoding", "expect"].map(String::from));
    }
    let authority = if port == 443 {
        name.to_string()
    } else {
        format!("{name}:{port}")
    };
    let mut lines = vec![format!("Host: {authority}")];
    lines.extend(
        head.lines
            .iter()
            .filter(|l| !dropped.contains(&http::name_of(l)))
            .cloned(),
    );
    lines.extend(rewritten.values().cloned());
    if let Some(value) = &value {
        lines.push(format!("{}: {value}", injection.header));
    }
    if let Some(body) = &body {
        lines.push(format!("Content-Length: {}", body.len()));
    }
    // A WebSocket (Develocity's test distribution) asks to keep the connection, as something else.
    let upgrade = headers.contains_key("upgrade")
        && http::tokens(headers.get("connection").map_or("", String::as_str))
            .iter()
            .any(|t| t == "upgrade");
    lines.push(format!(
        "Connection: {}",
        if upgrade { "Upgrade" } else { "close" }
    ));
    let mut upstream: Vec<u8> = format!("{} {logged} {}\r\n", head.method, head.version)
        .chars()
        .map(|c| c as u8)
        .collect();
    for line in &lines {
        upstream.extend(line.chars().map(|c| c as u8));
        upstream.extend(b"\r\n");
    }
    upstream.extend(b"\r\n");
    if let Some(body) = &body {
        upstream.extend(body);
    }
    Ok(Request {
        method: head.method.clone(),
        upstream,
        path,
        injected: value.is_some() || !rewritten.is_empty() || in_body,
    })
}

/// A request target's scheme and authority (an absolute form's), path and query.
pub(super) fn split_target(target: &str) -> (Option<String>, Option<String>, String, String) {
    let (rest, query) = target.split_once('?').map_or((target, ""), |(r, q)| (r, q));
    let query = query.split('#').next().unwrap_or("").to_string();
    if let Some((scheme, after)) = rest.split_once("://")
        && !scheme.contains('/')
    {
        let (authority, path) = after
            .find('/')
            .map_or((after, ""), |i| (&after[..i], &after[i..]));
        return (
            Some(scheme.to_lowercase()),
            Some(authority.to_string()),
            path.to_string(),
            query,
        );
    }
    (None, None, rest.to_string(), query)
}

/// An authority's host, lowercase, without its port or user.
pub(super) fn host_of(authority: &str) -> String {
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    host.to_lowercase()
}

/// All of a request's body, by its Content-Length or chunked; refused past MAX_BODY_BYTES, or
/// when it isn't all there. A bottle waiting to be told to send it (100-continue) is told here.
pub(super) fn request_body(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<u8>, Refused> {
    if http::tokens(headers.get("expect").map_or("", String::as_str))
        .iter()
        .any(|t| t == "100-continue")
    {
        let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    match http::read_body(reader, headers, MAX_BODY_BYTES) {
        Ok(Some(body)) => Ok(body),
        Ok(None) => Err(Refused::new(413, "Content Too Large", "bad-request")),
        Err(_) => Err(Refused::bad_request()),
    }
}

/// `body` with the credential in each rewrite's field that holds the stand-in, and whether one did:
/// only a JSON object's own fields, and only one that is exactly the stand-in.
pub(super) fn json_with_credential(
    body: &[u8],
    rewrites: &[&Rewrite],
    injection: &Injection,
) -> (Vec<u8>, bool) {
    let Ok(Value::Object(mut document)) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), false);
    };
    let mut any = false;
    for r in rewrites {
        if let Some(Value::String(v)) = document.get(&r.json)
            && presents(r, v, &injection.standin)
        {
            document.insert(r.json.clone(), encoded(r, &injection.credential).into());
            any = true;
        }
    }
    if any {
        (serde_json::to_vec(&Value::Object(document)).unwrap(), true)
    } else {
        (body.to_vec(), false)
    }
}

/// A 403 for a route a credential's feature refuses, saying so to whoever reads it: as JSON when
/// the request asked for it, in the fields API clients show (`message`, as GitHub's errors have it,
/// and Anthropic's `error.message`); else as text, which git shows as the remote's message.
pub(super) fn route_refused(
    headers: &BTreeMap<String, String>,
    why: &str,
    outcome: &'static str,
) -> Refused {
    let message = format!(
        "{why} This is the bottle's policy, not an outage or a bug: don't retry the request, or look for another \
         way to make it. If your task needs this route, stop and tell the user which request was refused and why \
         you need it: only they can allow it."
    );
    let wants_json = |h: &str| headers.get(h).is_some_and(|v| v.contains("json"));
    let (body, content_type) = if wants_json("accept") || wants_json("content-type") {
        let body = json!({"type": "error", "error": {"type": "permission_error", "message": message}, "message": message});
        (format!("{body}\n").into_bytes(), "application/json")
    } else {
        (format!("{message}\n").into_bytes(), "text/plain")
    };
    Refused {
        status: Some(403),
        reason: "Forbidden".into(),
        outcome,
        body: Some(body),
        content_type,
    }
}

// --- the host -----------------------------------------------------------------------
