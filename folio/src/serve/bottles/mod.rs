//! The bottles service: for a bottle with the list-bottles permission, `GET http://bottle.internal/`,
//! the other running bottles: each one's name, repo and labels; and for any, `POST
//! http://bottle.internal/activity`, its agent at work (activity.rs), whatever the body says.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;

use serde::Serialize;

use super::http;
use super::service::{Caller, Flow, Service};

/// The bottles service.
pub struct Bottles;

impl Service for Bottles {
    fn serve(&self, caller: &Caller, stream: UnixStream) -> Flow {
        // An event's body is read past, unread: it says nothing that counts.
        let (head, _, mut writer) = match http::read_request(stream, 64 * 1024) {
            Ok(request) => request,
            Err(outcome) => return Flow::new("bottles", outcome),
        };
        self.answer(caller, &head, head.path_and_query().0, &mut writer)
    }
}

impl Bottles {
    /// Answers a request read whole, for `path`.
    pub fn answer(
        &self,
        caller: &Caller,
        head: &http::Head,
        path: &str,
        mut writer: &mut UnixStream,
    ) -> Flow {
        if head.method == "POST" && path == "/activity" {
            return match crate::activity::record(&caller.bottle.name) {
                Ok(()) => {
                    http::respond(&mut writer, 204, "No Content", b"", "text/plain");
                    Flow::new("bottles", "activity")
                }
                Err(e) => {
                    http::respond(
                        &mut writer,
                        500,
                        "Internal Server Error",
                        format!("{e}\n").as_bytes(),
                        "text/plain",
                    );
                    Flow::new("bottles", "activity-failed")
                }
            };
        }
        if head.method != "GET" || path != "/" {
            http::respond(
                &mut writer,
                405,
                "Method Not Allowed",
                b"GET / for the running bottles, or POST /activity\n",
                "text/plain",
            );
            return Flow::new("bottles", "bad-request");
        }
        let bottle = caller.bottle;
        if !bottle.permissions.iter().any(|p| p == "list-bottles") {
            http::respond(
                &mut writer,
                403,
                "Forbidden",
                b"listing bottles needs the list-bottles permission\n",
                "text/plain",
            );
            return Flow::new("bottles", "refused");
        }
        /// A running bottle, as another with list-bottles sees it.
        #[derive(Serialize)]
        struct Peer<'a> {
            labels: &'a BTreeMap<String, String>,
            name: &'a str,
            repo: &'a Option<String>,
        }
        let all = caller.bound.all();
        let peers: Vec<Peer> = all
            .iter()
            .filter(|b| b.name != bottle.name)
            .map(|b| Peer {
                name: &b.name,
                repo: &b.repo,
                labels: &b.labels,
            })
            .collect();
        let peers = serde_json::to_string(&peers).expect("bottles are JSON");
        http::respond(
            &mut writer,
            200,
            "OK",
            format!("{peers}\n").as_bytes(),
            "application/json",
        );
        Flow::new("bottles", "bottles")
    }
}
