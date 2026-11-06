//! MCP поверх Streamable HTTP. Поддерживаются обе «эпохи» протокола:
//! современная (2026-07-28: версия и метаданные в `_meta` каждого запроса,
//! без сессий) и прежняя (2025-03-26…2025-11-25: рукопожатие `initialize`).

use base64::Engine as _;
use serde_json::{Value, json};

use super::{Hub, tools};

pub const MODERN_VERSION: &str = "2026-07-28";
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Имена заголовков — в нижнем регистре.
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    /// Порт, на котором слушает сервер (для проверки `Host`).
    pub port: u16,
}

impl Request<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Option<Value>,
}

fn error(status: u16, id: Option<&Value>, code: i64, message: impl Into<String>) -> Response {
    error_data(status, id, code, message, None)
}

fn error_data(
    status: u16,
    id: Option<&Value>,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Response {
    let mut err = json!({ "code": code, "message": message.into() });
    if let Some(d) = data {
        err["data"] = d;
    }
    let mut body = json!({ "jsonrpc": "2.0", "error": err });
    if let Some(id) = id {
        body["id"] = id.clone();
    }
    Response {
        status,
        body: Some(body),
    }
}

fn result(id: &Value, result: Value) -> Response {
    Response {
        status: 200,
        body: Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    }
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Защита от DNS rebinding: страница в браузере не должна достучаться до сервера.
fn origin_ok(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    is_local_host(host)
}

fn host_ok(host: &str, port: u16) -> bool {
    match host.rsplit_once(':') {
        Some((h, p)) => is_local_host(h) && p == port.to_string(),
        None => is_local_host(host),
    }
}

/// `Mcp-Name` может прийти как `=?base64?…?=`.
fn decode_header_value(v: &str) -> Option<String> {
    match v
        .strip_prefix("=?base64?")
        .and_then(|s| s.strip_suffix("?="))
    {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|b| String::from_utf8(b).ok()),
        None => Some(v.to_owned()),
    }
}

fn server_info() -> Value {
    json!({ "name": "tarn", "version": crate::update::current_version().to_string() })
}

const INSTRUCTIONS: &str = "\
Ты управляешь удалённым рабочим столом (обычно Windows) через RDP-клиент Tarn. \
Пользователь видит каждое твоё действие в окне клиента в реальном времени и может \
в любой момент остановить управление.\n\
- Начинай со screenshot. Координаты для click/move_mouse/drag/scroll/zoom — в пикселях \
последнего скриншота (его размер указан в ответе), а не реального экрана.\n\
- После действий делай screenshot и проверяй результат: экран обновляется не мгновенно, \
при необходимости используй wait.\n\
- Мелкий текст читай через zoom.\n\
- Текст печатай через type_text (сначала кликни в поле): символы уходят как Unicode, \
раскладка сервера не важна. Клавиши и сочетания (enter, esc, tab, ctrl+c, alt+tab, win+r) — \
press_keys. Ctrl/Shift-клик — параметр modifiers у click и drag.\n\
- Если инструмент управления вернул ошибку о режиме «только просмотр», не пытайся обойти \
её — попроси пользователя включить режим «Управление» в меню «🤖 Claude» в Tarn.\n\
- Не вводи пароли и не подтверждай необратимые действия без явной просьбы пользователя.";

fn capabilities() -> Value {
    json!({ "tools": {} })
}

pub fn handle(req: &Request, hub: &Hub) -> Response {
    let path = req.path.split('?').next().unwrap_or("");
    if path != "/mcp" {
        return Response {
            status: 404,
            body: None,
        };
    }
    if let Some(origin) = req.header("origin")
        && !origin_ok(origin)
    {
        return error(403, None, INVALID_REQUEST, "Origin не разрешён");
    }
    if !req.header("host").is_some_and(|h| host_ok(h, req.port)) {
        return error(403, None, INVALID_REQUEST, "Host не разрешён");
    }
    let token = req
        .header("authorization")
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .unwrap_or("");
    if !hub.token_matches(token.trim()) {
        return error(
            401,
            None,
            INVALID_REQUEST,
            "Нужен заголовок Authorization: Bearer <токен из Tarn>",
        );
    }
    if req.method != "POST" {
        return Response {
            status: 405,
            body: None,
        };
    }
    hub.note_request();

    let msg: Value = match serde_json::from_slice(req.body) {
        Ok(v) => v,
        Err(e) => return error(400, None, PARSE_ERROR, format!("Некорректный JSON: {e}")),
    };
    if !msg.is_object() {
        return error(400, None, INVALID_REQUEST, "Ожидается один JSON-RPC объект");
    }
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // Ответы от клиента в этой схеме не используются.
        return Response {
            status: 202,
            body: None,
        };
    };
    let Some(id) = msg.get("id").filter(|v| !v.is_null()) else {
        // Уведомление (notifications/initialized и т.п.) — принимаем молча.
        return Response {
            status: 202,
            body: None,
        };
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    let modern_version = params
        .get("_meta")
        .and_then(|m| m.get(META_VERSION))
        .and_then(Value::as_str);
    let modern = modern_version.is_some();
    if let Some(v) = modern_version {
        if v != MODERN_VERSION {
            let mut supported = vec![MODERN_VERSION];
            supported.extend_from_slice(LEGACY_VERSIONS);
            return error_data(
                400,
                Some(id),
                UNSUPPORTED_VERSION,
                "Unsupported protocol version",
                Some(json!({ "supported": supported, "requested": v })),
            );
        }
        if req.header("mcp-protocol-version") != Some(v) {
            return error(
                400,
                Some(id),
                HEADER_MISMATCH,
                "Header mismatch: MCP-Protocol-Version не совпадает с _meta",
            );
        }
        if req.header("mcp-method") != Some(method) {
            return error(
                400,
                Some(id),
                HEADER_MISMATCH,
                "Header mismatch: Mcp-Method не совпадает с method",
            );
        }
        if method == "tools/call" {
            let body_name = params.get("name").and_then(Value::as_str);
            let header_name = req.header("mcp-name").and_then(decode_header_value);
            if body_name.is_none() || header_name.as_deref() != body_name {
                return error(
                    400,
                    Some(id),
                    HEADER_MISMATCH,
                    "Header mismatch: Mcp-Name не совпадает с params.name",
                );
            }
        }
    }

    let complete = |mut v: Value| {
        if modern {
            v["resultType"] = json!("complete");
        }
        v
    };

    match method {
        "initialize" if !modern => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|v| LEGACY_VERSIONS.contains(v))
                .unwrap_or(LEGACY_VERSIONS[0]);
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": capabilities(),
                    "serverInfo": server_info(),
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "server/discover" => {
            let mut supported = vec![MODERN_VERSION];
            supported.extend_from_slice(LEGACY_VERSIONS);
            result(
                id,
                json!({
                    "resultType": "complete",
                    "supportedVersions": supported,
                    "capabilities": capabilities(),
                    "_meta": { "io.modelcontextprotocol/serverInfo": server_info() },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => result(id, complete(json!({}))),
        "tools/list" => result(id, complete(json!({ "tools": tools::list() }))),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return error(200, Some(id), INVALID_PARAMS, "Нет params.name");
            };
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match tools::call(hub, name, &args) {
                Some(out) => result(
                    id,
                    complete(json!({ "content": out.content, "isError": out.is_error })),
                ),
                None => error(
                    200,
                    Some(id),
                    INVALID_PARAMS,
                    format!("Неизвестный инструмент: {name}"),
                ),
            }
        }
        _ => error(
            if modern { 404 } else { 200 },
            Some(id),
            METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 47800;

    fn hub() -> Hub {
        let hub = Hub::new(|| {});
        hub.set_token("secret");
        hub
    }

    fn base_headers() -> Vec<(String, String)> {
        vec![
            ("host".into(), format!("127.0.0.1:{PORT}")),
            ("authorization".into(), "Bearer secret".into()),
        ]
    }

    fn call(hub: &Hub, headers: &[(String, String)], body: Value) -> Response {
        let body = body.to_string();
        handle(
            &Request {
                method: "POST",
                path: "/mcp",
                headers,
                body: body.as_bytes(),
                port: PORT,
            },
            hub,
        )
    }

    fn modern_headers(method: &str, name: Option<&str>) -> Vec<(String, String)> {
        let mut h = base_headers();
        h.push(("mcp-protocol-version".into(), MODERN_VERSION.into()));
        h.push(("mcp-method".into(), method.into()));
        if let Some(n) = name {
            h.push(("mcp-name".into(), n.into()));
        }
        h
    }

    fn modern_body(method: &str, mut params: Value) -> Value {
        params["_meta"] = json!({
            META_VERSION: MODERN_VERSION,
            "io.modelcontextprotocol/clientInfo": { "name": "t", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
    }

    #[test]
    fn legacy_handshake_and_tools() {
        let hub = hub();
        let h = base_headers();
        let r = call(
            &hub,
            &h,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}),
        );
        assert_eq!(r.status, 200);
        let res = &r.body.unwrap()["result"];
        assert_eq!(res["protocolVersion"], "2025-06-18");
        assert!(res["capabilities"]["tools"].is_object());

        let n = call(
            &hub,
            &h,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        assert_eq!(n.status, 202);
        assert!(n.body.is_none());

        let list = call(
            &hub,
            &h,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = list.body.unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert!(tools.iter().any(|t| t["name"] == "screenshot"));
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));

        // Неизвестная версия → отдаём нашу последнюю legacy.
        let r = call(
            &hub,
            &h,
            json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2024-01-01"}}),
        );
        assert_eq!(
            r.body.unwrap()["result"]["protocolVersion"],
            LEGACY_VERSIONS[0]
        );
    }

    #[test]
    fn modern_requests_validate_headers() {
        let hub = hub();
        let r = call(
            &hub,
            &modern_headers("tools/list", None),
            modern_body("tools/list", json!({})),
        );
        assert_eq!(r.status, 200);
        let body = r.body.unwrap();
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["id"], 7);

        let d = call(
            &hub,
            &modern_headers("server/discover", None),
            modern_body("server/discover", json!({})),
        );
        assert_eq!(
            d.body.unwrap()["result"]["supportedVersions"][0],
            MODERN_VERSION
        );

        // Mcp-Method не совпадает.
        let r = call(
            &hub,
            &modern_headers("ping", None),
            modern_body("tools/list", json!({})),
        );
        assert_eq!(r.status, 400);
        assert_eq!(r.body.unwrap()["error"]["code"], HEADER_MISMATCH);

        // Mcp-Name обязателен для tools/call; base64-форма декодируется.
        let body = modern_body(
            "tools/call",
            json!({"name":"wait","arguments":{"seconds":0}}),
        );
        let r = call(&hub, &modern_headers("tools/call", None), body.clone());
        assert_eq!(r.status, 400);
        let r = call(
            &hub,
            &modern_headers("tools/call", Some("=?base64?d2FpdA==?=")),
            body,
        );
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert_eq!(r.body.unwrap()["result"]["isError"], false);

        // Неизвестный метод в modern — 404 с JSON-RPC ошибкой.
        let r = call(
            &hub,
            &modern_headers("foo/bar", None),
            modern_body("foo/bar", json!({})),
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.body.unwrap()["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn unsupported_modern_version_lists_supported() {
        let hub = hub();
        let mut body = modern_body("tools/list", json!({}));
        body["params"]["_meta"][META_VERSION] = json!("2099-01-01");
        let r = call(&hub, &modern_headers("tools/list", None), body);
        assert_eq!(r.status, 400);
        let err = &r.body.unwrap()["error"];
        assert_eq!(err["code"], UNSUPPORTED_VERSION);
        assert_eq!(err["data"]["requested"], "2099-01-01");
    }

    #[test]
    fn rejects_bad_auth_origin_host_and_get() {
        let hub = hub();
        let ping = json!({"jsonrpc":"2.0","id":1,"method":"ping"});

        let mut h = base_headers();
        h[1].1 = "Bearer wrong".into();
        assert_eq!(call(&hub, &h, ping.clone()).status, 401);

        let mut h = base_headers();
        h.push(("origin".into(), "https://evil.example".into()));
        assert_eq!(call(&hub, &h, ping.clone()).status, 403);

        let mut h = base_headers();
        h.push(("origin".into(), "http://localhost:3000".into()));
        assert_eq!(call(&hub, &h, ping.clone()).status, 200);

        let mut h = base_headers();
        h[0].1 = "evil.example:47800".into();
        assert_eq!(call(&hub, &h, ping.clone()).status, 403);

        let h = base_headers();
        let get = handle(
            &Request {
                method: "GET",
                path: "/mcp",
                headers: &h,
                body: b"",
                port: PORT,
            },
            &hub,
        );
        assert_eq!(get.status, 405);
    }

    #[test]
    fn origin_parsing() {
        assert!(origin_ok("http://127.0.0.1:47800"));
        assert!(origin_ok("http://[::1]:5"));
        assert!(!origin_ok("http://localhost.evil.com"));
        assert!(!origin_ok("null"));
    }
}
