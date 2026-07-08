// ══════════════════════════════════════════════════════════════════
// CONFIGURAÇÃO DE DEPLOY — edite aqui e recompile
// ══════════════════════════════════════════════════════════════════

/// Senha permanente para aceitar conexões remotas.
/// Vazio ("") = não força senha (comportamento padrão).
/// Ignorado quando a licença é validada com sucesso (a senha diária assume o controle).
pub const PASSWORD: &str = "";

/// IPs autorizados a conectar, separados por vírgula.
/// Suporta notação CIDR (ex: "192.168.1.0/24") e IPs individuais.
/// Vazio ("") = qualquer IP pode conectar.
pub const WHITELIST: &str = "";

/// URL completa da rota de checagem de licença (api/license/check).
/// Contrato esperado:
///   Request  (POST, JSON): { "uuid": "...", "hostname": "..." }
///   Response (JSON):       { "valid": bool,
///                            "id_server": "...",     // servidor de ID (compartilhado entre clientes)
///                            "relay_server": "...",  // servidor de relay (idem)
///                            "key": "...",            // chave do par acima
///                            "expires_at": "31/12/2026",  // opcional, string já formatada p/ exibição
///                            "message": "..." }            // opcional, mostrado quando valid=false
///
/// NENHUM valor aqui identifica o cliente — o mesmo binário roda em todos os
/// 40+ clientes. Quem identifica o dispositivo é o uuid/hostname (gerados
/// automaticamente pelo próprio RustDesk, sem edição manual); é o servidor
/// que decide, do lado dele, a qual cliente/licença aquele uuid pertence.
pub const LICENSE_API_URL: &str =
    "https://my.infomek.com.br/api/rustdesk/receiver.php?_path=api/license/check";

/// Base da mesma API acima (sem o receiver.php/_path), aplicada como
/// "api-server" do RustDesk — é o que faz o client mandar login/heartbeat/
/// sysinfo/auditoria de conexão (server/connection.rs) para o nosso backend
/// em vez de cair no padrão público (admin.rustdesk.com, bloqueado) ou
/// derivar a porta interna do id_server. Roteado por .htaccess em
/// api/rustdesk/ (qualquer api/rustdesk/api/xxx -> receiver.php?_path=api/xxx).
/// Mesma infra compartilhada de LICENSE_API_URL, nada por cliente.
const API_SERVER_URL: &str = "https://my.infomek.com.br/api/rustdesk";

// ── Aplicado automaticamente ao iniciar ───────────────────────────

/// Cache local (por dispositivo, via LocalConfig — não sincroniza/exporta)
/// do último id_server/relay_server/key recebidos com sucesso da API.
/// Serve só para tolerar instabilidades pontuais de rede em dispositivos que
/// JÁ validaram com sucesso alguma vez — nunca é usado no lugar de uma
/// validação, só como ponte durante uma queda de conexão. Formato: 3 linhas
/// separadas por "\n" (id_server, relay_server, key).
const LICENSE_SERVER_CACHE_KEY: &str = "deploy-license-server-cfg";

/// Data de vencimento (texto livre, como veio da API). Limpa quando a
/// licença é negada explicitamente, para não mostrar data velha.
const LICENSE_EXPIRY_KEY: &str = "deploy-license-expires-at";

#[derive(serde::Deserialize, Default)]
struct LicenseCheckResponse {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    id_server: String,
    #[serde(default)]
    relay_server: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    expires_at: String,
    #[serde(default)]
    message: String,
}

struct ServerConfig {
    id_server: String,
    relay_server: String,
    key: String,
}

enum LicenseOutcome {
    /// Licença confirmada (nesta checagem ou reaproveitada do cache local),
    /// com a config de servidor deste dispositivo/cliente específico.
    Valid(ServerConfig),
    /// O servidor negou explicitamente (chave revogada/expirada).
    Denied { message: String },
    /// Sem resposta do servidor (rede/instabilidade) e sem nenhum cache local
    /// de uma validação anterior bem-sucedida neste dispositivo — ou seja,
    /// não existe nenhuma config de servidor própria pra aplicar. Bloqueia
    /// em vez de deixar o app cair no servidor público do RustDesk.
    Blocked { message: String },
}

/// Senha do dia: Inf0mek@ + data corrente no formato ddMMyy.
/// Ex.: 2026-07-02 → "Inf0mek@020726".
fn daily_password() -> String {
    format!("Inf0mek@{}", chrono::Local::now().format("%d%m%y"))
}

/// Consulta a API de licenciamento a partir da identidade do próprio
/// dispositivo (uuid/hostname) — nada é fixado no fonte. O servidor decide,
/// do lado dele, a qual cliente/licença aquele uuid/hostname pertence, e
/// devolve o id_server/relay_server/key compartilhados da infraestrutura.
fn check_license() -> LicenseOutcome {
    use hbb_common::config::LocalConfig;

    let uuid = crate::ui_interface::get_uuid();
    let hostname = crate::common::hostname();

    let body = serde_json::json!({
        "uuid": uuid,
        "hostname": hostname,
    });

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return license_from_cache(),
    };

    match client.post(LICENSE_API_URL).json(&body).send() {
        // Resposta recebida do servidor: ele decidiu, não é falha de rede.
        Ok(resp) => match resp.json::<LicenseCheckResponse>() {
            Ok(parsed) if !parsed.valid => {
                LocalConfig::set_option(LICENSE_SERVER_CACHE_KEY.to_owned(), "".to_owned());
                LocalConfig::set_option(LICENSE_EXPIRY_KEY.to_owned(), "".to_owned());
                let message = if parsed.message.is_empty() {
                    "Licença inválida ou expirada. Contate o suporte Infomek.".to_owned()
                } else {
                    parsed.message
                };
                LicenseOutcome::Denied { message }
            }
            // valid=true mas sem id_server: resposta incompleta/inesperada,
            // trata como instabilidade da API, não como negação.
            Ok(parsed) if parsed.id_server.is_empty() => license_from_cache(),
            Ok(parsed) => {
                let cfg = format!(
                    "{}\n{}\n{}",
                    parsed.id_server, parsed.relay_server, parsed.key
                );
                LocalConfig::set_option(LICENSE_SERVER_CACHE_KEY.to_owned(), cfg);
                if !parsed.expires_at.is_empty() {
                    LocalConfig::set_option(LICENSE_EXPIRY_KEY.to_owned(), parsed.expires_at);
                }
                LicenseOutcome::Valid(ServerConfig {
                    id_server: parsed.id_server,
                    relay_server: parsed.relay_server,
                    key: parsed.key,
                })
            }
            // JSON incompleto/inesperado: trata como instabilidade da API, não
            // como negação de licença.
            Err(_) => license_from_cache(),
        },
        // Falha de rede/conexão: pode ser instabilidade pontual da API, não uma
        // negação da licença — cai no cache pra não travar o técnico à toa.
        Err(_) => license_from_cache(),
    }
}

fn license_from_cache() -> LicenseOutcome {
    use hbb_common::config::LocalConfig;

    let blocked = || LicenseOutcome::Blocked {
        message: "Sem conexão com o servidor de licenciamento e nenhuma validação anterior \
                  registrada neste dispositivo. Verifique a internet e tente novamente."
            .to_owned(),
    };

    let cached = LocalConfig::get_option(LICENSE_SERVER_CACHE_KEY);
    if cached.is_empty() {
        return blocked();
    }
    let mut parts = cached.splitn(3, '\n');
    let id_server = parts.next().unwrap_or_default().to_owned();
    let relay_server = parts.next().unwrap_or_default().to_owned();
    let key = parts.next().unwrap_or_default().to_owned();
    if id_server.is_empty() {
        blocked()
    } else {
        LicenseOutcome::Valid(ServerConfig {
            id_server,
            relay_server,
            key,
        })
    }
}

/// Mostra um alerta nativo (sem depender do Flutter já estar carregado) antes
/// de encerrar o processo por licença negada/bloqueada.
fn show_license_blocked_dialog(message: &str) {
    let text = format!("{}!", message.trim_end_matches('!'));

    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::message_box(&text);
    }

    #[cfg(target_os = "macos")]
    {
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display dialog \"{}\" with title \"Infomek — Licença\" buttons {{\"OK\"}} default button \"OK\" with icon caution",
            escaped
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status();
    }

    #[cfg(target_os = "linux")]
    {
        let zenity = std::process::Command::new("zenity")
            .arg("--error")
            .arg("--title=Infomek — Licença")
            .arg(format!("--text={}", text))
            .status();
        if zenity.is_err() {
            let _ = std::process::Command::new("kdialog")
                .arg("--title")
                .arg("Infomek — Licença")
                .arg("--error")
                .arg(&text)
                .status();
        }
    }

    hbb_common::log::error!("{}", text);
}

pub fn apply() {
    use hbb_common::config;

    // Insere em OVERWRITE_SETTINGS → a UI exibe como "fixo" (sem checkbox editável)
    {
        let mut overwrite = config::OVERWRITE_SETTINGS.write().unwrap();

        if !WHITELIST.is_empty() {
            overwrite.insert("whitelist".to_owned(), WHITELIST.to_owned());
        }

        if !PASSWORD.is_empty() {
            // Força o modo "somente senha permanente" e bloqueia na UI
            overwrite.insert(
                "verification-method".to_owned(),
                "use-permanent-password".to_owned(),
            );
        }
    }

    // Grava a senha no Config (equivale a clicar "Set permanent password")
    if !PASSWORD.is_empty() {
        hbb_common::config::Config::set_permanent_password(PASSWORD);
    }

    // ── Licenciamento: valida este dispositivo via API (só uuid/hostname,
    // gerados automaticamente — nada fixo no fonte, o mesmo binário serve
    // todos os clientes), aplica o id_server/relay/key da infraestrutura
    // (compartilhados) e troca para a senha diária Inf0mek@ddMMyy. Em
    // qualquer cenário que não resulte em uma config de servidor válida
    // (negação explícita, ou sem rede e sem cache de uma validação anterior),
    // avisa o usuário e encerra — nunca deixa o app seguir e cair no
    // servidor público do RustDesk.
    match check_license() {
        LicenseOutcome::Valid(server) => {
            {
                let mut overwrite = config::OVERWRITE_SETTINGS.write().unwrap();
                if !server.id_server.is_empty() {
                    overwrite.insert(
                        "custom-rendezvous-server".to_owned(),
                        server.id_server,
                    );
                }
                if !server.relay_server.is_empty() {
                    overwrite.insert("relay-server".to_owned(), server.relay_server);
                }
                if !server.key.is_empty() {
                    overwrite.insert("key".to_owned(), server.key);
                }
                overwrite.insert("api-server".to_owned(), API_SERVER_URL.to_owned());
                overwrite.insert(
                    "verification-method".to_owned(),
                    "use-permanent-password".to_owned(),
                );
            }
            hbb_common::config::Config::set_permanent_password(&daily_password());
        }
        LicenseOutcome::Denied { message } => {
            show_license_blocked_dialog(&message);
            std::process::exit(1);
        }
        LicenseOutcome::Blocked { message } => {
            show_license_blocked_dialog(&message);
            std::process::exit(1);
        }
    }
}
