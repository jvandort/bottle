//! bottle.host, as a bottle made before policy 4 still reaches it (bottles/mod.rs's NETWORK): its
//! machine routes 192.0.2.1:80 to this socket, and asks for /git/<mount>/<remote>, /messages and
//! /bottles, which are answered by the services that have their own names now. Kept until no
//! running bottle has an older policy: each is given policy 4 as it next starts.

use std::os::unix::net::UnixStream;

use super::bottles::Bottles;
use super::git::Git;
use super::http;
use super::messages::Messages;
use super::service::{Caller, Flow, Service};

/// The most of a request's body read: a push's pack.
const MAX_BODY: usize = 64 * 1024 * 1024;

pub struct Host {
    pub git: Git,
}

impl Service for Host {
    fn serve(&self, caller: &Caller, stream: UnixStream) -> Flow {
        let (head, body, mut writer) = match http::read_request(stream, MAX_BODY) {
            Ok(request) => request,
            Err(outcome) => return Flow::new("host", outcome),
        };
        let (path, query) = head.path_and_query();
        if let Some(rest) = path.strip_prefix("/git/") {
            let path = format!("/{rest}");
            self.git
                .answer(caller, &head, &path, query, &body, &mut writer)
        } else if path == "/messages" {
            Messages.answer(caller, &head, "/", &body, &mut writer)
        } else if path == "/bottles" {
            Bottles.answer(caller, &head, "/", &mut writer)
        } else {
            http::respond(
                &mut writer,
                404,
                "Not Found",
                b"no such bottle.host service\n",
                "text/plain",
            );
            Flow::new("host", "not-found")
        }
    }
}
