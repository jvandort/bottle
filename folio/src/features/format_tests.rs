//! The feature format: the features folio ships, and what a feature's devcontainer-feature.json may
//! say, as folio loads, resolves and builds them. Each feature is read as its JSON (`derived_json`):
//! what it is, and what its options make of it.

use crate::features::TEST_DIR;

use super::feature::Feature;
use super::load::{available, load};
use super::resolve::{parse_spec, resolve};
use super::route;

use super::Result;

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::images;

/// The features in `dir`, for as long as this is held, on this test's thread.
struct Features {
    root: PathBuf,
    _tmp: Option<crate::tempdir::Dir>,
}

impl Features {
    /// The features folio ships.
    fn shipped() -> Features {
        let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/features"));
        TEST_DIR.with(|d| *d.borrow_mut() = Some(root.clone()));
        Features { root, _tmp: None }
    }

    /// None yet, in a directory of their own.
    fn empty() -> Features {
        let tmp = crate::tempdir::Dir::new();
        let root = tmp.path().to_path_buf();
        TEST_DIR.with(|d| *d.borrow_mut() = Some(root.clone()));
        Features {
            root,
            _tmp: Some(tmp),
        }
    }

    /// Feature `id`, its devcontainer-feature.json `spec` with an id and version, and an install.sh
    /// unless not `install`.
    fn feature(&self, id: &str, spec: Value) {
        self.feature_with(id, spec, true);
    }

    fn feature_with(&self, id: &str, spec: Value, install: bool) {
        let dir = self.root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let mut full = json!({"id": id, "version": "1.0.0"});
        for (k, v) in spec.as_object().unwrap() {
            full[k] = v.clone();
        }
        fs::write(dir.join("devcontainer-feature.json"), full.to_string()).unwrap();
        if install {
            fs::write(dir.join("install.sh"), "#!/bin/sh\n").unwrap();
        }
    }

    fn remove(&self, id: &str) {
        fs::remove_dir_all(self.root.join(id)).unwrap();
    }
}

impl Drop for Features {
    fn drop(&mut self) {
        TEST_DIR.with(|d| *d.borrow_mut() = None);
    }
}

/// A feature, its options at their defaults.
fn load_json(id: &str) -> Result<Value> {
    if id.contains(':') || id.contains(',') {
        return Err(format!("no feature named '{id}'"));
    }
    show(id)
}

/// A feature, with the options SPEC sets.
fn show(spec: &str) -> Result<Value> {
    let (id, settings) = parse_spec(spec)?;
    Ok(load(&id)?.configured(&settings)?.derived_json())
}

fn strings(specs: &[&str]) -> Vec<String> {
    specs.iter().map(|s| s.to_string()).collect()
}

/// The requested features and everything they depend on, in install order.
fn resolve_json(specs: &[&str]) -> Result<Vec<Value>> {
    Ok(resolve(&strings(specs))?
        .iter()
        .map(Feature::derived_json)
        .collect())
}

/// The one feature SPEC resolves to.
fn one(spec: &str) -> Value {
    let mut all = resolve_json(&[spec]).unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// Its credentials, filled in from its options, or why they can't be.
fn resolved_credentials(feature: &Value) -> Result<Vec<Value>> {
    match feature.get("unresolved") {
        Some(why) => Err(why.as_str().unwrap().to_string()),
        None => Ok(feature["resolved_credentials"].as_array().unwrap().clone()),
    }
}

fn apt_packages(feature: &Value) -> Result<Vec<Value>> {
    match feature.get("unresolved") {
        Some(why) => Err(why.as_str().unwrap().to_string()),
        None => Ok(feature["apt_packages"].as_array().unwrap().clone()),
    }
}

fn option_env(feature: &Value) -> Value {
    feature["option_env"].clone()
}

fn ids(features: &[Value]) -> Vec<String> {
    features
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect()
}

/// What builds the image of the features SPECS resolve to (the base image, for none): its tag,
/// inputs and recipe, as JSON.
fn image(specs: &[&str]) -> Value {
    let (tag, inputs, recipe) = if specs.is_empty() {
        (
            images::tag(crate::features::BASE),
            images::base_inputs().unwrap(),
            images::base().unwrap(),
        )
    } else {
        let features = resolve(&strings(specs)).unwrap();
        (
            images::image_tag(&features),
            images::inputs_hash(&features).unwrap(),
            images::image(&features).unwrap(),
        )
    };
    json!({"tag": tag, "inputs": inputs, "recipe": recipe})
}

/// What folio serve does with a request for `path` by `method` to SPEC's credential's hosts.
fn route_of(spec: &str, method: &str, path: &str) -> Value {
    let credentials = route(spec, method, path).unwrap()["credentials"].clone();
    assert_eq!(credentials.as_array().unwrap().len(), 1);
    credentials[0].clone()
}

fn fails(result: Result<impl std::fmt::Debug>, said: &str) {
    match result {
        Ok(v) => panic!("expected an error saying {said:?}, got {v:?}"),
        Err(e) => assert!(e.contains(said), "{e:?} doesn't say {said:?}"),
    }
}

fn hosts(credential: &Value) -> Vec<String> {
    credential["inject"]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap().to_string())
        .collect()
}

// --- the features folio ships ---------------------------------------------------------------

#[test]
fn every_shipped_feature_is_valid() {
    let _f = Features::shipped();
    assert_eq!(
        available(),
        [
            "cc",
            "chromium",
            "claude",
            "develocity",
            "github",
            "jvm",
            "python",
            "rust",
            "teamcity",
            "tools"
        ]
    );
    for id in available() {
        load_json(&id).unwrap_or_else(|e| panic!("{id}: {e}"));
    }
}

#[test]
fn every_shipped_feature_tells_the_agent_about_itself() {
    let _f = Features::shipped();
    for id in available() {
        let spec = if id == "develocity" || id == "teamcity" {
            format!("{id}:server=x")
        } else {
            id.clone()
        };
        let all = resolve_json(&[&spec]).unwrap();
        let feature = all.iter().find(|f| f["id"] == id.as_str()).unwrap();
        let context = feature["agent_context"].as_str().unwrap();
        assert!(!context.is_empty(), "{id}");
        assert!(!context.contains("${"), "{id}: {context}");
    }
}

#[test]
fn chromium_brings_python() {
    let _f = Features::shipped();
    assert_eq!(
        ids(&resolve_json(&["chromium"]).unwrap()),
        ["python", "chromium"]
    );
    let all = resolve_json(&["chromium:playwright=1.65.0"]).unwrap();
    assert!(
        all[1]["agent_context"]
            .as_str()
            .unwrap()
            .contains("Playwright 1.65.0")
    );
}

#[test]
fn claude_options() {
    let _f = Features::shipped();
    assert_eq!(
        option_env(&load_json("claude").unwrap()),
        json!({"PERMISSIONMODE": "bypassPermissions", "THEME": "dark", "TUI": "default"})
    );
    fails(
        resolve_json(&["claude:permissionMode=yolo"]),
        "option permissionMode must be one of",
    );
}

#[test]
fn claude_renders_without_capturing_the_mouse() {
    // Fullscreen would copy a selection to a clipboard a bottle hasn't got.
    let _f = Features::shipped();
    assert_eq!(option_env(&load_json("claude").unwrap())["TUI"], "default");
    assert_eq!(
        option_env(&one("claude:tui=fullscreen"))["TUI"],
        "fullscreen"
    );
    fails(
        resolve_json(&["claude:tui=full"]),
        "option tui must be one of default, fullscreen",
    );
}

#[test]
fn claude_talks_to_the_api_through_the_proxy_and_holds_no_token() {
    let _f = Features::shipped();
    let claude = load_json("claude").unwrap();
    let credentials = resolved_credentials(&claude).unwrap();
    let credential = &credentials[0];
    assert_eq!(hosts(credential), ["api.anthropic.com"]);
    assert_eq!(
        (
            &credential["inject"]["header"],
            &credential["inject"]["value"]
        ),
        (&json!("Authorization"), &json!("Bearer ${credential}"))
    );
    // The API's own https address, which the proxy terminates to attach the real token, for every
    // call, not just the ones ANTHROPIC_BASE_URL moves; and a stand-in for the one Claude Code
    // needs to believe it's logged in, which folio sets per bottle, not in the image.
    assert!(claude["container_env"].get("ANTHROPIC_BASE_URL").is_none());
    assert_eq!(credential["inject"]["standin"], "CLAUDE_CODE_OAUTH_TOKEN");
    assert!(
        claude["container_env"]
            .get("CLAUDE_CODE_OAUTH_TOKEN")
            .is_none()
    );
}

#[test]
fn github_attaches_its_token_for_git_and_gh() {
    let _f = Features::shipped();
    let github = load_json("github").unwrap();
    let credential = resolved_credentials(&github).unwrap().remove(0);
    // github.com for git, api.github.com for gh and the REST and GraphQL APIs, uploads.github.com
    // for release assets.
    assert_eq!(
        hosts(&credential),
        ["github.com", "api.github.com", "uploads.github.com"]
    );
    assert_eq!(
        (
            &credential["inject"]["header"],
            &credential["inject"]["value"]
        ),
        (&json!("Authorization"), &json!("Bearer ${credential}"))
    );
    assert_eq!(credential["inject"]["standin"], "GH_TOKEN");
    assert!(github["container_env"].get("GH_TOKEN").is_none());
}

#[test]
fn routes_that_mint_export_or_grant_credentials_are_forbidden() {
    // The report's critical and high rows (2026-10-25), on each feature's hosts.
    let _f = Features::shipped();
    for (id, method, path) in [
        ("develocity", "POST", "/api/auth/token"),
        ("develocity", "POST", "/api/auth/revoke-signing-keys"),
        (
            "develocity",
            "POST",
            "/provision-access-key/init/gradle/4.6.1/host",
        ),
        ("develocity", "GET", "/provision-access-key/poll/abc"),
        (
            "develocity",
            "GET",
            "/api/test-distribution/agent-pools/p/registration-keys",
        ),
        (
            "develocity",
            "POST",
            "/api/test-distribution/agent-pools/p/registration-keys",
        ),
        ("develocity", "PUT", "/api/test-distribution/agent-pools/p"),
        ("develocity", "POST", "/api/build-cache/nodes/n/secret"),
        ("develocity", "POST", "/api/build-cache/nodes/n/purge"),
        ("develocity", "PUT", "/api/projects/p"),
        ("github", "POST", "/user/keys"),
        ("github", "POST", "/user/ssh_signing_keys"),
        ("github", "POST", "/repos/o/r/keys"),
        (
            "github",
            "POST",
            "/repos/o/r/actions/runners/registration-token",
        ),
        (
            "github",
            "POST",
            "/orgs/o/actions/runners/generate-jitconfig",
        ),
        ("github", "POST", "/user/codespaces"),
        ("github", "POST", "/repos/o/r/pulls/1/codespaces"),
        ("github", "POST", "/user/codespaces/c/exports"),
        ("github", "GET", "/copilot_internal/v2/token"),
        ("github", "POST", "/login/device/code"),
        ("github", "POST", "/login/oauth/access_token"),
        ("github", "POST", "/repos/o/r/dispatches"),
        (
            "github",
            "POST",
            "/repos/o/r/actions/workflows/ci.yml/dispatches",
        ),
        ("github", "PUT", "/repos/o/r/collaborators/someone"),
        ("github", "POST", "/orgs/o/personal-access-token-requests"),
        ("github", "PUT", "/repos/o/r/actions/secrets/KEY"),
        ("github", "PUT", "/user/codespaces/secrets/KEY"),
        (
            "github",
            "PATCH",
            "/repos/o/r/environments/prod/variables/X",
        ),
        ("github", "PUT", "/orgs/o/actions/oidc/customization/sub"),
        ("github", "POST", "/repos/o/r/hooks"),
        ("github", "POST", "/app/installations/1/access_tokens"),
        ("claude", "POST", "/api/oauth/claude_cli/create_api_key"),
        ("claude", "POST", "/v1/oauth/token"),
        ("claude", "POST", "/api/oauth/cri/token"),
        ("claude", "POST", "/api/oauth/local_pairing/attest"),
        (
            "claude",
            "POST",
            "/api/oauth/organizations/o/sync/github/auth",
        ),
        (
            "claude",
            "POST",
            "/api/oauth/organizations/o/prepaid/bundles",
        ),
        ("claude", "PATCH", "/api/oauth/account/settings"),
        ("claude", "GET", "/v1/organizations/api_keys"),
    ] {
        assert_eq!(
            route_of(id, method, path)["forbidden"],
            true,
            "{id} {method} {path}"
        );
    }
}

#[test]
fn what_the_tools_need_is_not_forbidden() {
    let _f = Features::shipped();
    for (id, method, path) in [
        ("develocity", "POST", "/mcp"),
        ("develocity", "GET", "/api/builds"),
        ("develocity", "POST", "/scans/publish/gradle/4.6.1/token"),
        ("develocity", "PUT", "/cache/0123abcd"),
        // What gh, git and Claude Code were seen asking for with it (bottled's log, 2026-10-27).
        ("github", "GET", "/"),
        ("github", "GET", "/user"),
        ("github", "GET", "/rate_limit"),
        ("github", "GET", "/search/repositories"),
        ("github", "GET", "/repos/o/r/pulls/1"),
        ("github", "GET", "/repos/o/r/actions/runs/1/jobs"),
        ("github", "GET", "/repos/o/r/contents/README.md"),
        // A moved repo: GitHub redirects /repos/o/r to it by ID (gh api
        // repos/containers/libkrunfw/..., 2026-11-12).
        (
            "github",
            "GET",
            "/repositories/287064639/releases/tags/v5.6.2",
        ),
        ("github", "POST", "/graphql"),
        ("github", "GET", "/o/r.git/info/refs"),
        ("github", "POST", "/o/r/git-upload-pack"),
        ("claude", "POST", "/v1/messages"),
        ("claude", "POST", "/api/event_logging/v2/batch"),
        ("claude", "POST", "/api/eval/sdk-abc"),
        ("claude", "GET", "/api/claude_cli/bootstrap"),
        ("claude", "GET", "/api/claude_code/settings"),
        ("claude", "GET", "/api/claude_code/policy_limits"),
    ] {
        let route = route_of(id, method, path);
        assert_eq!(
            (&route["forbidden"], &route["allowed"]),
            (&json!(false), &json!(true)),
            "{id} {method} {path}"
        );
    }
}

#[test]
fn the_rest_doesnt_get_the_credential() {
    let _f = Features::shipped();
    for (id, method, path) in [
        ("github", "POST", "/repos/o/r/pulls"),
        ("github", "POST", "/repos/o/r/issues/1/comments"),
        ("github", "PUT", "/repos/o/r/pulls/1/merge"),
        ("github", "DELETE", "/repos/o/r"),
        ("github", "POST", "/o/r.git/git-receive-pack"),
        ("github", "POST", "/o/r.git/info/lfs/objects/batch"),
        ("github", "GET", "/user/installations"),
        ("github", "POST", "/repos/o/r/releases/1/assets"),
        ("claude", "GET", "/api/oauth/usage"),
        (
            "claude",
            "GET",
            "/api/oauth/organizations/o/sync/github/auth",
        ),
        ("claude", "POST", "/v1/code/github/import-token"),
        ("claude", "GET", "/v1/vaults/v/credentials"),
        ("claude", "POST", "/api/frame/upload"),
    ] {
        assert_eq!(
            route_of(id, method, path)["allowed"],
            false,
            "{id} {method} {path}"
        );
    }
}

#[test]
fn develocity_attaches_its_key_only_where_its_tools_use_it() {
    let _f = Features::shipped();
    for (method, path) in [
        ("POST", "/api/builds"),
        ("GET", "/admin"),
        ("GET", "/scans/publish/gradle/4.6.1/token"),
    ] {
        assert_eq!(
            route_of("develocity", method, path)["allowed"],
            false,
            "{method} {path}"
        );
    }
    let develocity = load_json("develocity").unwrap();
    // Where Gradle's Develocity plugin presents it other than as a header's token.
    let mut rewrites: Vec<Value> = develocity["credentials"][0]["inject"]["rewrite"]
        .as_array()
        .unwrap()
        .clone();
    rewrites.sort_by_key(|r| r["route"].as_str().unwrap().to_string());
    assert_eq!(
        rewrites,
        [
            json!({"route": "GET /distribution-broker/**", "header": "X-User-Access-Key", "json": "", "encoding": "base64"}),
            json!({"route": "POST /scans/publish/*/*/token", "header": "", "json": "accessKey", "encoding": "raw"}),
        ]
    );
}

#[test]
fn github_installs_after_tools_which_owns_etc_gitconfig() {
    let _f = Features::shipped();
    assert_eq!(
        ids(&resolve_json(&["github", "tools"]).unwrap()),
        ["tools", "github"]
    );
}

#[test]
fn teamcity_needs_a_server_and_attaches_its_token_there() {
    let _f = Features::shipped();
    let teamcity = one("teamcity:server=https://ci.corp.example.com");
    let credential = resolved_credentials(&teamcity).unwrap().remove(0);
    assert_eq!(hosts(&credential), ["ci.corp.example.com"]);
    assert_eq!(
        (
            &credential["inject"]["header"],
            &credential["inject"]["value"]
        ),
        (&json!("Authorization"), &json!("Bearer ${credential}"))
    );
    fails(
        resolve_json(&["teamcity"]),
        "feature teamcity needs its server option",
    );
}

#[test]
fn teamcity_is_read_only_until_asked_otherwise() {
    let _f = Features::shipped();
    assert_eq!(
        option_env(&one("teamcity:server=ci.example.com"))["READONLY"],
        "true"
    );
    assert_eq!(
        option_env(&one("teamcity:server=ci.example.com,readOnly=false"))["READONLY"],
        "false"
    );
}

#[test]
fn jvm_options() {
    let _f = Features::shipped();
    assert_eq!(
        option_env(&load_json("jvm").unwrap()),
        json!({"VERSION": "25", "ADDITIONALVERSIONS": ""})
    );
    assert_eq!(
        option_env(&one("jvm:version=21,additionalVersions=17,11")),
        json!({"VERSION": "21", "ADDITIONALVERSIONS": "17,11"})
    );
}

#[test]
fn python_options() {
    let _f = Features::shipped();
    assert_eq!(
        option_env(&load_json("python").unwrap()),
        json!({"EXTERNALLYMANAGED": "false"})
    );
    assert_eq!(
        option_env(&one("python:externallyManaged=true")),
        json!({"EXTERNALLYMANAGED": "true"})
    );
    fails(
        resolve_json(&["python:externallyManaged=yes"]),
        "option externallyManaged is true or false",
    );
}

// --- what a feature's definition may say ---------------------------------------------------

/// Feature a, as `spec` defines it, refused for saying what `said` says.
fn invalid(said: &str, spec: Value) {
    invalid_with(said, spec, true);
}

fn invalid_with(said: &str, spec: Value, install: bool) {
    let f = Features::empty();
    f.feature_with("a", spec, install);
    fails(load_json("a"), said);
}

#[test]
fn a_minimal_feature() {
    let f = Features::empty();
    f.feature("a", json!({}));
    let a = load_json("a").unwrap();
    assert_eq!(
        (
            &a["id"],
            &a["version"],
            &a["options"],
            &a["container_env"],
            &a["depends_on"]
        ),
        (
            &json!("a"),
            &json!("1.0.0"),
            &json!({}),
            &json!({}),
            &json!([])
        )
    );
}

#[test]
fn metadata_is_accepted() {
    let f = Features::empty();
    f.feature("a", json!({"name": "A", "description": "d", "documentationURL": "u", "licenseURL": "l", "keywords": ["k"]}));
    load_json("a").unwrap();
}

#[test]
fn options_are_its_install_scripts_environment() {
    let f = Features::empty();
    f.feature(
        "a",
        json!({"options": {
            "version": {"type": "string", "default": "21", "proposals": ["17", "21"]},
            "with-docs": {"type": "boolean", "default": false, "description": "d"},
        }}),
    );
    assert_eq!(
        option_env(&load_json("a").unwrap()),
        json!({"VERSION": "21", "WITH_DOCS": "false"})
    );
}

#[test]
fn unsupported_keys_fail_loudly() {
    invalid(
        "a/devcontainer-feature.json: unsupported: mounts, privileged",
        json!({"mounts": [], "privileged": true}),
    );
}

#[test]
fn lifecycle_hooks_are_unsupported() {
    invalid(
        "unsupported: postCreateCommand",
        json!({"postCreateCommand": "echo hi"}),
    );
}

#[test]
fn its_id_must_be_its_directorys() {
    invalid("id must be \"a\"", json!({"id": "b"}));
}

#[test]
fn a_version_is_required() {
    invalid("version is required", json!({"version": ""}));
}

#[test]
fn an_install_sh_is_required() {
    invalid_with("install.sh is missing", json!({}), false);
}

#[test]
fn option_types() {
    invalid(
        "type must be one of string, boolean",
        json!({"options": {"x": {"type": "number", "default": 1}}}),
    );
}

#[test]
fn an_options_default_must_match_its_type() {
    invalid(
        "needs a boolean default",
        json!({"options": {"x": {"type": "boolean", "default": "true"}}}),
    );
}

#[test]
fn an_options_default_is_required() {
    invalid(
        "needs a string default",
        json!({"options": {"x": {"type": "string"}}}),
    );
}

#[test]
fn an_options_default_is_in_its_enum() {
    invalid(
        "default isn't in its enum",
        json!({"options": {"x": {"type": "string", "default": "c", "enum": ["a", "b"]}}}),
    );
}

#[test]
fn unsupported_option_keys() {
    invalid(
        "option \"x\": unsupported: format",
        json!({"options": {"x": {"type": "string", "default": "", "format": "uri"}}}),
    );
}

#[test]
fn container_env_values_are_strings() {
    invalid(
        "containerEnv must map names to strings",
        json!({"containerEnv": {"X": 1}}),
    );
}

#[test]
fn container_env_names() {
    invalid(
        "invalid variable name \"1X\"",
        json!({"containerEnv": {"1X": "v"}}),
    );
}

#[test]
fn container_env_values_are_as_they_are() {
    invalid(
        "references to other variables",
        json!({"containerEnv": {"PATH": "/opt/bin:${PATH}"}}),
    );
}

#[test]
fn remote_dependencies_are_unsupported() {
    invalid(
        "remote features aren't supported",
        json!({"dependsOn": {"ghcr.io/devcontainers/features/node:1": {}}}),
    );
}

#[test]
fn dependency_options_are_unsupported() {
    let f = Features::empty();
    f.feature("b", json!({}));
    f.feature("a", json!({"dependsOn": {"b": {"version": "2"}}}));
    fails(load_json("a"), "options for dependencies aren't supported");
}

#[test]
fn an_unknown_dependency() {
    invalid(
        "dependsOn: no feature named \"ghost\"",
        json!({"dependsOn": {"ghost": {}}}),
    );
}

#[test]
fn an_unknown_installs_after() {
    invalid(
        "installsAfter: no feature named \"ghost\"",
        json!({"installsAfter": ["ghost"]}),
    );
}

/// customizations for one credential, attached to ci.example.com unless `spec` says otherwise.
fn credential_spec(spec: Value) -> Value {
    let mut base = json!({"description": "A token", "inject": {"hosts": ["ci.example.com"], "value": "Bearer ${credential}"}});
    for (k, v) in spec.as_object().unwrap() {
        base[k] = v.clone();
    }
    json!({"bottle": {"credentials": {"tok": base}}})
}

#[test]
fn credentials() {
    let f = Features::empty();
    f.feature("a", json!({"customizations": credential_spec(json!({}))}));
    let a = load_json("a").unwrap();
    assert_eq!(a["credentials"][0]["name"], "tok");
    assert_eq!(hosts(&a["credentials"][0]), ["ci.example.com"]);
}

#[test]
fn a_credential_injected_at_a_hosts_option() {
    let f = Features::empty();
    f.feature("a", json!({
        "options": {"server": {"type": "string", "default": "ci.example.com"}},
        "customizations": credential_spec(json!({"inject": {
            "hosts": ["${server}", "*.mirror.example.com"], "value": "token ${credential}", "header": "X-Auth"}})),
    }));
    let c = resolved_credentials(&load_json("a").unwrap())
        .unwrap()
        .remove(0);
    assert_eq!(hosts(&c), ["ci.example.com", "*.mirror.example.com"]);
    assert_eq!(
        (&c["inject"]["header"], &c["inject"]["value"]),
        (&json!("X-Auth"), &json!("token ${credential}"))
    );
    let configured = one("a:server=other.example.com");
    assert_eq!(
        hosts(&resolved_credentials(&configured).unwrap()[0])[0],
        "other.example.com"
    );
}

#[test]
fn a_credential_read_from_a_properties_file_an_option_names() {
    let f = Features::empty();
    f.feature(
        "a",
        json!({
            "options": {"keys": {"type": "string", "default": "~/.tool/keys.properties"}},
            "customizations": credential_spec(json!({"properties": "${keys}"})),
        }),
    );
    assert_eq!(
        resolved_credentials(&load_json("a").unwrap()).unwrap()[0]["properties"],
        "~/.tool/keys.properties"
    );
    let configured = one("a:keys=/elsewhere/keys.properties");
    assert_eq!(
        resolved_credentials(&configured).unwrap()[0]["properties"],
        "/elsewhere/keys.properties"
    );
}

/// Feature a, its credential attached to the hosts `host` (with ${server} in it) names.
fn server_feature(f: &Features, host: &str) {
    f.feature("a", json!({
        "options": {"server": {"type": "string", "default": ""}},
        "customizations": credential_spec(json!({"inject": {"hosts": [host], "value": "Bearer ${credential}"}})),
    }));
}

#[test]
fn a_host_that_is_only_an_option_is_each_server_it_lists() {
    let f = Features::empty();
    server_feature(&f, "${server}");
    let several = one("a:server=https://one.example.com, two.example.com/,");
    assert_eq!(
        hosts(&resolved_credentials(&several).unwrap()[0]),
        ["one.example.com", "two.example.com"]
    );
    for spec in ["a:server=,", "a:server=one.example.com,x y"] {
        assert!(resolved_credentials(&one(spec)).is_err(), "{spec}");
    }
}

#[test]
fn a_list_only_stands_for_a_whole_host() {
    let f = Features::empty();
    server_feature(&f, "${server}:8443");
    fails(
        resolved_credentials(&one("a:server=one.example.com,two.example.com")),
        "must be a host",
    );
}

#[test]
fn an_option_holding_a_server_may_be_written_as_a_url() {
    let f = Features::empty();
    server_feature(&f, "${server}");
    assert_eq!(
        hosts(&resolved_credentials(&one("a:server=https://ci.example.com/")).unwrap()[0]),
        ["ci.example.com"]
    );
    fails(
        resolved_credentials(&one("a:server=http://ci.example.com")),
        "must be a host, reached over https",
    );
}

#[test]
fn a_credential_belongs_to_hosts_and_never_to_the_bottle() {
    for (spec, said) in [
        (
            json!({}),
            "needs inject, the hosts the proxy attaches it to",
        ),
        // A bottle can't be handed one any more.
        (json!({"env": "TOKEN"}), "unsupported: env"),
    ] {
        let f = Features::empty();
        let mut credential = json!({"description": "d"});
        for (k, v) in spec.as_object().unwrap() {
            credential[k] = v.clone();
        }
        f.feature(
            "a",
            json!({"customizations": {"bottle": {"credentials": {"tok": credential}}}}),
        );
        fails(load_json("a"), said);
    }
}

#[test]
fn a_cache() {
    let f = Features::empty();
    f.feature(
        "a",
        json!({"customizations": {"bottle": {"cache": {
        "hosts": ["repo.example.com", "*.cdn.example.com"], "exclude": ["*/index.json"]}}}}),
    );
    assert_eq!(
        load_json("a").unwrap()["cache"],
        json!({"hosts": ["repo.example.com", "*.cdn.example.com"], "exclude": ["*/index.json"]})
    );
    f.feature("b", json!({}));
    assert!(load_json("b").unwrap()["cache"].is_null());
}

#[test]
fn a_cache_is_hosts_and_paths() {
    for (cache, said) in [
        (json!([]), "cache must be an object"),
        (json!({}), "cache.hosts must be a non-empty list"),
        (json!({"hosts": []}), "cache.hosts must be a non-empty list"),
        (
            json!({"hosts": ["https://repo.example.com"]}),
            "must be a lowercase host name",
        ),
        (
            json!({"hosts": ["repo.example.com:8443"]}),
            "must be a lowercase host name",
        ),
        (
            json!({"hosts": ["Repo.example.com"]}),
            "must be a lowercase host name",
        ),
        (
            json!({"hosts": ["${server}"]}),
            "must be a lowercase host name",
        ),
        (
            json!({"hosts": ["repo.example.com"], "exclude": ["index.json"]}),
            "starting with / or *",
        ),
        (
            json!({"hosts": ["repo.example.com"], "ttl": 60}),
            "cache: unsupported: ttl",
        ),
    ] {
        invalid(
            said,
            json!({"customizations": {"bottle": {"cache": cache}}}),
        );
    }
}

#[test]
fn required_options() {
    let f = Features::empty();
    f.feature(
        "a",
        json!({"options": {"server": {"type": "string", "default": ""}},
                          "customizations": {"bottle": {"requiredOptions": ["server"]}}}),
    );
    assert_eq!(
        load_json("a").unwrap()["required_options"],
        json!(["server"])
    );
    fails(
        resolve_json(&["a"]),
        "feature a needs its server option: --feature a:server=",
    );
    fails(
        resolve_json(&["a:server="]),
        "its server option can't be empty",
    );
    assert_eq!(
        one("a:server=ci.example.com")["values"],
        json!({"server": "ci.example.com"})
    );
}

#[test]
fn a_required_option_is_what_the_user_set_not_what_it_defaults_to() {
    // The placeholder default is never a value: setting the option to the same string is still
    // setting it, and the feature says so.
    let f = Features::empty();
    f.feature(
        "a",
        json!({"options": {"server": {"type": "string", "default": "ci.example.com"}},
                          "customizations": {"bottle": {"requiredOptions": ["server"]}}}),
    );
    assert_eq!(
        one("a:server=ci.example.com")["spec"],
        "a:server=ci.example.com"
    );
    fails(resolve_json(&["a"]), "needs its server option");
}

#[test]
fn required_options_must_be_options() {
    invalid(
        "requiredOptions: no option \"ghost\"",
        json!({"customizations": {"bottle": {"requiredOptions": ["ghost"]}}}),
    );
}

#[test]
fn a_credential_injected_at_an_option_nobody_set() {
    let f = Features::empty();
    server_feature(&f, "${server}");
    fails(
        resolved_credentials(&load_json("a").unwrap()),
        "isn't set: use --feature a:server=",
    );
}

#[test]
fn what_inject_may_say() {
    let routes = |extra: Value| -> Value {
        let mut inject = json!({"hosts": ["x"], "value": "Bearer ${credential}"});
        for (k, v) in extra.as_object().unwrap() {
            inject[k] = v.clone();
        }
        inject
    };
    for (inject, said) in [
        (
            json!({"hosts": [], "value": "Bearer ${credential}"}),
            "inject.hosts must be a non-empty list",
        ),
        (
            json!({"hosts": ["${nope}"], "value": "Bearer ${credential}"}),
            "names no option of this feature",
        ),
        (
            json!({"hosts": ["${flag}"], "value": "Bearer ${credential}"}),
            "option \"flag\" is a boolean, not a host",
        ),
        (
            json!({"hosts": ["x"], "value": "Bearer"}),
            "must be the header value, containing ${credential}",
        ),
        (
            routes(json!({"header": "bad header"})),
            "must be a header name",
        ),
        (
            json!({"hosts": ["https://x"], "value": "Bearer ${credential}"}),
            "always reached over https",
        ),
        (
            routes(json!({"scheme": "http"})),
            "inject: unsupported: scheme",
        ),
        (
            routes(json!({"forbid": "POST /x"})),
            "inject.forbid must be a list of routes",
        ),
        (
            routes(json!({"forbid": ["/x"]})),
            "inject.forbid: \"/x\" isn't METHOD",
        ),
        (
            routes(json!({"allow": ["GET x"]})),
            "inject.allow: \"GET x\" isn't METHOD",
        ),
        (
            routes(json!({"allow": ["GET,PUT /x"]})),
            "methods are names separated by",
        ),
        (
            routes(json!({"forbid": ["GET /{x"]})),
            "a brace without its pair",
        ),
        (
            routes(json!({"forbid": ["* /api/auth/**"], "allow": ["GET /api/**"]})),
            "\"GET /api/**\" is allowed, but reaches \"* /api/auth/**\", which is forbidden: narrow it",
        ),
        (
            routes(
                json!({"forbid": ["POST /api/auth/token"], "rewrite": [{"route": "POST /api/*/*", "json": "key"}]}),
            ),
            "\"POST /api/*/*\" is allowed, but reaches",
        ),
        (
            routes(json!({"rewrite": {}})),
            "inject.rewrite must be a list",
        ),
        (
            routes(json!({"rewrite": [{"route": "GET /x"}]})),
            "needs a header or a json field, not both",
        ),
        (
            routes(json!({"rewrite": [{"route": "GET /x", "header": "X-Key", "json": "key"}]})),
            "needs a header or a json field, not both",
        ),
        (
            routes(json!({"rewrite": [{"route": "GET /x", "header": "X Key"}]})),
            "isn't a header name",
        ),
        (
            routes(json!({"rewrite": [{"route": "GET /x", "json": "key", "encoding": "hex"}]})),
            "encoding is raw or base64",
        ),
        (
            routes(json!({"rewrite": [{"route": "GET /x", "json": "key", "where": "body"}]})),
            "unsupported: where",
        ),
    ] {
        invalid(
            said,
            json!({"options": {"flag": {"type": "boolean", "default": false}},
                             "customizations": credential_spec(json!({"inject": inject}))}),
        );
    }
}

#[test]
fn a_context_fills_in_options() {
    let f = Features::empty();
    f.feature("a", json!({"options": {"server": {"type": "string", "default": ""}, "ro": {"type": "boolean", "default": true}},
                          "customizations": {"bottle": {"context": "Talks to ${server}; read-only: ${ro}.\n"}}}));
    assert_eq!(
        one("a:server=ci.example.com")["agent_context"],
        "Talks to ci.example.com; read-only: true."
    );
}

#[test]
fn what_a_context_may_say() {
    invalid(
        "context must be a string",
        json!({"customizations": {"bottle": {"context": ["x"]}}}),
    );
    invalid(
        "context: ${ghost} names no option",
        json!({"customizations": {"bottle": {"context": "${ghost}"}}}),
    );
}

#[test]
fn no_context_says_nothing() {
    let f = Features::empty();
    f.feature("a", json!({}));
    assert_eq!(load_json("a").unwrap()["agent_context"], "");
}

#[test]
fn other_tools_customizations_fail_loudly() {
    invalid(
        "customizations: unsupported: vscode",
        json!({"customizations": {"vscode": {"extensions": []}}}),
    );
}

#[test]
fn unknown_bottle_customizations() {
    invalid(
        "customizations.bottle: unsupported: ports",
        json!({"customizations": {"bottle": {"ports": []}}}),
    );
}

#[test]
fn what_a_credential_may_say() {
    for (spec, said) in [
        (json!({"description": ""}), "needs a description"),
        (json!({"file": "/x"}), "unsupported: file"),
        (
            json!({"inject": {"hosts": ["x"], "value": "Bearer ${credential}", "standin": "1BAD"}}),
            "inject.standin must be an environment variable name",
        ),
        (
            json!({"login": {"command": ["x"], "capture": "(x)"}}),
            "unsupported: login",
        ),
        (
            json!({"properties": ""}),
            "properties must be the path of a properties file",
        ),
        (json!({"properties": "${nope}"}), "names no string option"),
    ] {
        invalid(said, json!({"customizations": credential_spec(spec)}));
    }
}

#[test]
fn invalid_json() {
    let f = Features::empty();
    fs::create_dir_all(f.root.join("a")).unwrap();
    fs::write(f.root.join("a/devcontainer-feature.json"), "{nope").unwrap();
    fails(load_json("a"), "invalid JSON");
}

#[test]
fn an_unknown_feature() {
    let f = Features::empty();
    f.feature("a", json!({}));
    fails(load_json("nope"), "no feature named \"nope\"; available: a");
}

// --- options ------------------------------------------------------------------------------

/// Feature f, with a string, a boolean and an enum option.
fn with_options() -> Features {
    let f = Features::empty();
    f.feature(
        "f",
        json!({"options": {
            "version": {"type": "string", "default": "25"},
            "docs": {"type": "boolean", "default": false},
            "flavor": {"type": "string", "default": "a", "enum": ["a", "b"]},
        }}),
    );
    f
}

#[test]
fn bad_specs() {
    let _f = with_options();
    for spec in ["f:", "f:version", "f:=1", "f:,x=1", "f:version=1,version=2"] {
        assert!(resolve_json(&[spec]).is_err(), "{spec}");
    }
}

#[test]
fn values_and_spec() {
    let _f = with_options();
    let f = one("f:version=17,docs=true");
    assert_eq!(
        f["values"],
        json!({"version": "17", "docs": true, "flavor": "a"})
    );
    // Canonical: sorted.
    assert_eq!(f["spec"], "f:docs=true,version=17");
    assert_eq!(
        option_env(&f),
        json!({"VERSION": "17", "DOCS": "true", "FLAVOR": "a"})
    );
}

#[test]
fn defaults_are_left_out_of_the_spec() {
    let _f = with_options();
    assert_eq!(one("f:version=25,flavor=a")["spec"], "f");
}

#[test]
fn an_unknown_option() {
    let _f = with_options();
    fails(
        resolve_json(&["f:nope=1"]),
        "feature f has no option \"nope\" (options: docs, flavor, version)",
    );
}

#[test]
fn boolean_values() {
    let _f = with_options();
    fails(
        resolve_json(&["f:docs=yes"]),
        "option docs is true or false, not \"yes\"",
    );
}

#[test]
fn an_enum() {
    let _f = with_options();
    fails(
        resolve_json(&["f:flavor=c"]),
        "option flavor must be one of a, b, not \"c\"",
    );
}

#[test]
fn the_same_feature_with_different_options() {
    let _f = with_options();
    fails(
        resolve_json(&["f:version=17", "f:version=21"]),
        "feature f requested twice with different options",
    );
}

#[test]
fn dependencies_take_defaults_unless_requested() {
    let f = with_options();
    f.feature("g", json!({"dependsOn": {"f": {}}}));
    let specs = |all: Vec<Value>| -> Vec<String> {
        all.iter()
            .map(|x| x["spec"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(specs(resolve_json(&["g"]).unwrap()), ["f", "g"]);
    assert_eq!(
        specs(resolve_json(&["g", "f:version=17"]).unwrap()),
        ["f:version=17", "g"]
    );
}

// --- install order -------------------------------------------------------------------------

#[test]
fn ties_break_by_id() {
    let f = Features::empty();
    for name in ["c", "a", "b"] {
        f.feature(name, json!({}));
    }
    assert_eq!(
        ids(&resolve_json(&["c", "a", "b"]).unwrap()),
        ["a", "b", "c"]
    );
}

#[test]
fn depends_on_pulls_in_and_orders_first() {
    let f = Features::empty();
    f.feature("tools", json!({}));
    f.feature("jvm", json!({"dependsOn": {"tools": {}}}));
    assert_eq!(ids(&resolve_json(&["jvm"]).unwrap()), ["tools", "jvm"]);
}

#[test]
fn transitive_dependencies() {
    let f = Features::empty();
    f.feature("c", json!({}));
    f.feature("b", json!({"dependsOn": {"c": {}}}));
    f.feature("a", json!({"dependsOn": {"b": {}}}));
    assert_eq!(ids(&resolve_json(&["a"]).unwrap()), ["c", "b", "a"]);
}

#[test]
fn installs_after_orders_only_when_present() {
    let f = Features::empty();
    f.feature("z", json!({}));
    f.feature("a", json!({"installsAfter": ["z"]}));
    assert_eq!(ids(&resolve_json(&["a"]).unwrap()), ["a"]);
    assert_eq!(ids(&resolve_json(&["a", "z"]).unwrap()), ["z", "a"]);
}

#[test]
fn a_cycle() {
    let f = Features::empty();
    for (me, other) in [("a", "b"), ("b", "a")] {
        let dir = f.root.join(me);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("install.sh"), "").unwrap();
        fs::write(
            dir.join("devcontainer-feature.json"),
            json!({"id": me, "version": "1", "dependsOn": {other: {}}}).to_string(),
        )
        .unwrap();
    }
    fails(resolve_json(&["a"]), "cycle: a, b");
}

#[test]
fn duplicates_install_once() {
    let f = Features::empty();
    f.feature("tools", json!({}));
    f.feature("jvm", json!({"dependsOn": {"tools": {}}}));
    assert_eq!(
        ids(&resolve_json(&["jvm", "tools", "jvm"]).unwrap()),
        ["tools", "jvm"]
    );
}

// --- images -------------------------------------------------------------------------------

#[test]
fn a_tag_is_canonical() {
    let f = Features::empty();
    f.feature("tools", json!({}));
    f.feature("claude", json!({}));
    let tag = image(&["tools", "claude"])["tag"].clone();
    assert_eq!(tag, "localhost/bottle/base:with-claude.tools");
    assert_eq!(image(&["claude", "tools"])["tag"], tag);
    assert_eq!(images::image_tag(&[]), "localhost/bottle/base:latest");
}

#[test]
fn a_tag_distinguishes_options() {
    let f = Features::empty();
    f.feature(
        "jvm",
        json!({"options": {"version": {"type": "string", "default": "25"}}}),
    );
    let default = image(&["jvm"])["tag"].as_str().unwrap().to_string();
    assert_eq!(default, "localhost/bottle/base:with-jvm");
    // Setting an option to its default changes nothing.
    assert_eq!(image(&["jvm:version=25"])["tag"], default.as_str());
    let other = image(&["jvm:version=17"])["tag"]
        .as_str()
        .unwrap()
        .to_string();
    let hash = other
        .strip_prefix("localhost/bottle/base:with-jvm-")
        .unwrap();
    assert!(
        hash.len() == 8
            && hash
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{other}"
    );
}

#[test]
fn a_recipe() {
    let f = Features::empty();
    f.feature("tools", json!({"containerEnv": {"EDITOR": "vim"}}));
    f.feature("jvm", json!({"dependsOn": {"tools": {}}, "options": {"version": {"type": "string", "default": "21"}},
                            "containerEnv": {"JAVA_HOME": "/opt/jdk", "JDK": "21"}}));
    let recipe = image(&["jvm"])["recipe"].clone();
    assert_eq!(recipe["from"], "localhost/bottle/base:latest");
    assert_eq!(recipe["labels"], json!({"bottle.features": "jvm tools"}));
    let steps = recipe["steps"].as_array().unwrap();
    let [tools, editor, _, jvm, java_env, mirror] = &steps[..] else {
        panic!("{steps:?}")
    };
    // Each install.sh where it is, in the features directory mounted for the build: tools first.
    assert_eq!(
        (&tools["cwd"], &jvm["cwd"]),
        (
            &json!(format!("{}/tools", images::MOUNT)),
            &json!(format!("{}/jvm", images::MOUNT))
        )
    );
    assert_eq!(jvm["run"], json!(["/bin/sh", "./install.sh"]));
    assert_eq!(jvm["env"]["_REMOTE_USER"], "genie");
    assert_eq!(jvm["env"]["VERSION"], "21");
    assert_eq!(editor, &json!({"env": {"EDITOR": "vim"}}));
    assert_eq!(
        java_env,
        &json!({"env": {"JAVA_HOME": "/opt/jdk", "JDK": "21"}})
    );
    let last = mirror["run"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(
        last.contains("printf '%s\\n' \"JDK=$JDK\" >> /etc/environment"),
        "{last}"
    );
}

#[test]
fn option_values_go_as_they_are() {
    let f = Features::empty();
    f.feature(
        "a",
        json!({"options": {"flags": {"type": "string", "default": "a b; rm -rf /"}}}),
    );
    let recipe = image(&["a"])["recipe"].clone();
    // An environment, not a shell line.
    assert_eq!(recipe["steps"][0]["env"]["FLAGS"], "a b; rm -rf /");
    assert_eq!(recipe["steps"].as_array().unwrap().len(), 1);
}

// --- what features install with apt: declared, installed in one step first ------------------

fn repository(overrides: Value) -> Value {
    let mut repository = json!({"uri": "https://apt.example/stable", "suite": "stable", "components": ["main"], "key": "example.asc"});
    for (k, v) in overrides.as_object().unwrap() {
        repository[k] = v.clone();
    }
    repository
}

/// Feature `id`, installing `apt`, with each repository's key, and more of `spec`.
fn apt_feature(f: &Features, id: &str, apt: Value, spec: Value) {
    let mut full = json!({"customizations": {"bottle": {"apt": apt.clone()}}});
    for (k, v) in spec.as_object().unwrap() {
        full[k] = v.clone();
    }
    f.feature(id, full);
    for repository in apt["repositories"].as_array().into_iter().flatten() {
        fs::write(
            f.root.join(id).join(repository["key"].as_str().unwrap()),
            "a key",
        )
        .unwrap();
    }
}

#[test]
fn one_step_first_every_repository_then_every_package_once() {
    let f = Features::empty();
    apt_feature(&f, "tools", json!({"packages": ["curl", "jq"]}), json!({}));
    apt_feature(
        &f,
        "claude",
        json!({"repositories": [repository(json!({}))], "packages": ["claude-code", "jq"]}),
        json!({"dependsOn": {"tools": {}}}),
    );
    let recipe = image(&["claude"])["recipe"].clone();
    let steps = recipe["steps"].as_array().unwrap();
    let [apt_step, tools, claude] = &steps[..] else {
        panic!("{steps:?}")
    };
    let script = apt_step["run"][2].as_str().unwrap();
    for said in [
        format!(
            "install -D -m 0644 {}/claude/example.asc /etc/apt/keyrings/example.asc",
            images::MOUNT
        ),
        "deb [signed-by=/etc/apt/keyrings/example.asc] https://apt.example/stable stable main"
            .into(),
        "apt-get $options install -y --no-install-recommends curl jq claude-code\n".into(),
        "Acquire::https::apt.example::CaInfo".into(),
    ] {
        assert!(script.contains(&said), "{said} in {script}");
    }
    assert_eq!(script.matches("apt-get $options update").count(), 1);
    // The build's to fill in.
    assert_eq!(apt_step["env"][images::BUILD_CA], "");
    assert_eq!(
        (&tools["cwd"], &claude["cwd"]),
        (
            &json!(format!("{}/tools", images::MOUNT)),
            &json!(format!("{}/claude", images::MOUNT))
        )
    );
}

#[test]
fn without_any_theres_no_apt_step() {
    let f = Features::empty();
    f.feature("tools", json!({}));
    let recipe = image(&["tools"])["recipe"].clone();
    assert_eq!(recipe["steps"].as_array().unwrap().len(), 1);
    assert_eq!(
        recipe["steps"][0]["cwd"],
        format!("{}/tools", images::MOUNT)
    );
}

#[test]
fn a_package_from_options_is_one_for_each_value() {
    let f = Features::empty();
    apt_feature(
        &f,
        "jvm",
        json!({"packages": ["temurin-${version}-jdk", "temurin-${additionalVersions}-jdk"]}),
        json!({"options": {"version": {"type": "string", "default": "25"},
                                   "additionalVersions": {"type": "string", "default": ""}}}),
    );
    assert_eq!(
        apt_packages(&one("jvm")).unwrap(),
        [json!("temurin-25-jdk")]
    );
    assert_eq!(
        apt_packages(&one("jvm:version=21,additionalVersions=17, 11")).unwrap(),
        [
            json!("temurin-21-jdk"),
            json!("temurin-17-jdk"),
            json!("temurin-11-jdk")
        ]
    );
    fails(
        apt_packages(&one("jvm:version=21 && curl x")),
        "isn't an apt package's name",
    );
}

#[test]
fn a_feature_says_its_apt_as_folio_takes_it() {
    for (apt, said) in [
        (json!({"packages": "jq"}), "packages must be a list"),
        (json!({"packages": ["jq; rm"]}), "packages must be a list"),
        (json!({"packages": ["${nope}"]}), "${nope} names no option"),
        (
            json!({"repositories": [{"uri": "https://apt.example"}]}),
            "each is an object of",
        ),
        (
            json!({"repositories": [repository(json!({"uri": "ftp://apt.example"}))]}),
            "http:// or https://",
        ),
        (
            json!({"repositories": [repository(json!({"components": []}))]}),
            "components must be a non-empty list",
        ),
        (
            json!({"repositories": [repository(json!({"key": "../key.asc"}))]}),
            "the name of a file",
        ),
        (json!({"sources": []}), "unsupported: sources"),
    ] {
        let f = Features::empty();
        f.feature("bad", json!({"customizations": {"bottle": {"apt": apt}}}));
        fails(load_json("bad"), said);
        f.remove("bad");
    }
    let f = Features::empty();
    f.feature(
        "keyless",
        json!({"customizations": {"bottle": {"apt": {"repositories": [repository(json!({}))]}}}}),
    );
    fails(load_json("keyless"), "key \"example.asc\" is missing");
}

#[test]
fn its_repositories_are_kept_for_its_bottles_and_builds() {
    let f = Features::empty();
    apt_feature(
        &f,
        "claude",
        json!({"repositories": [repository(json!({}))], "packages": ["claude-code"]}),
        json!({}),
    );
    f.feature("base", json!({}));
    for kept in [
        images::cached_for(&resolve(&strings(&["claude"])).unwrap()),
        images::build_cached().unwrap(),
    ] {
        let by_host = |host: &str| kept.iter().find(|c| c.host == host).cloned();
        assert_eq!(by_host("apt.example").unwrap().include, ["/stable/*.deb"]);
        assert!(by_host("deb.debian.org").is_some());
    }
}

#[test]
fn base_isnt_a_feature_to_depend_on() {
    let f = Features::empty();
    f.feature("base", json!({}));
    assert!(!available().contains(&"base".to_string()));
    fails(resolve_json(&["base"]), "base is in every bottle");
    f.feature("needy", json!({"dependsOn": {"base": {}}}));
    fails(load_json("needy"), "installed on base already");
}
