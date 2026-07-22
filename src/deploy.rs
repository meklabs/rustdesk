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

/// Timestamp Unix (segundos, UTC) da última vez que a API confirmou
/// `valid: true` neste dispositivo. Limpo junto com o cache acima quando a
/// licença é negada explicitamente. `license_from_cache()` usa isso pra medir
/// há quanto tempo o dispositivo está operando sem contato bem-sucedido com a
/// API — ver LICENSE_GRACE_PERIOD_SECS.
const LICENSE_LAST_VALIDATED_KEY: &str = "deploy-license-last-validated";

/// Prazo máximo que uma validação em cache pode ser reaproveitada sem contato
/// bem-sucedido com a API (3,5 dias = 302400s). Depois disso, mesmo com cache
/// válido, o app bloqueia — evita que uma licença revogada continue
/// funcionando pra sempre só porque o dispositivo ficou sem rede (de
/// propósito ou não). Cobre folgas de fim de semana/feriado prolongado ou uma
/// queda de link/manutenção da API sem derrubar o técnico no meio do serviço.
const LICENSE_GRACE_PERIOD_SECS: i64 = 302_400;

/// Exit code usado quando `apply()` encerra o processo por licença negada ou
/// bloqueada (nunca por um crash comum). O serviço Windows (`run_service()`
/// em platform/windows.rs) relança o `--server` automaticamente sempre que
/// ele morre, sem nenhum limite — sem esse código dedicado, uma licença
/// bloqueada vira um loop infinito de "relança -> mostra diálogo -> sai ->
/// relança" a cada ~300ms. `run_service()` reconhece esse código específico
/// e para de relançar até o próximo restart do serviço.
pub const LICENSE_BLOCKED_EXIT_CODE: i32 = 42;

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
    /// Licença confirmada nesta checagem, direto com a API (online).
    Valid(ServerConfig),
    /// Sem resposta da API agora, mas reaproveitando uma validação
    /// anterior bem-sucedida deste dispositivo, ainda dentro do prazo de
    /// tolerância (LICENSE_GRACE_PERIOD_SECS). `grace_expires_at` é quando
    /// esse prazo acaba — usado só pra avisar o usuário, não bloqueia nada
    /// ainda.
    CachedGrace {
        server: ServerConfig,
        grace_expires_at: chrono::DateTime<chrono::Local>,
    },
    /// O servidor negou explicitamente (chave revogada/expirada).
    Denied { message: String },
    /// Sem resposta do servidor (rede/instabilidade) e sem nenhuma validação
    /// aproveitável: ou nunca validou com sucesso neste dispositivo, ou o
    /// cache existe mas passou do prazo de tolerância (LICENSE_GRACE_PERIOD_SECS)
    /// sem contato novo com a API. Bloqueia em vez de deixar o app seguir sem
    /// uma config de servidor própria ou cair no servidor público do RustDesk.
    Blocked { message: String },
}

/// Senha do dia: Inf0mek@ + data corrente no formato ddMMyy.
/// Ex.: 2026-07-02 → "Inf0mek@020726".
fn daily_password() -> String {
    format!("Inf0mek@{}", chrono::Local::now().format("%d%m%y"))
}

/// `apply()` só roda uma vez, quando o processo inicia — mas o serviço do
/// RustDesk fica de pé em segundo plano por dias sem reiniciar. Sem isso, a
/// senha diária calculada em `daily_password()` nunca era recalculada depois
/// do boot: a senha ficava congelada na data em que o processo subiu pela
/// última vez, e a "senha de hoje" nunca era aplicada. Este timer roda pra
/// sempre em background, checando a cada 5min se o dia mudou, e só então
/// reaplica `set_permanent_password`. Não reconsulta a API de licença — é só
/// o rollover da data, então não derruba uma sessão em andamento se a
/// licença tiver sido revogada nesse meio-tempo (isso só é reavaliado no
/// próximo restart do processo).
fn spawn_daily_password_refresher() {
    std::thread::spawn(|| {
        let mut last_applied = chrono::Local::now().format("%d%m%y").to_string();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(300));
            let today = chrono::Local::now().format("%d%m%y").to_string();
            if today != last_applied {
                hbb_common::config::Config::set_permanent_password(&daily_password());
                last_applied = today;
            }
        }
    });
}

/// Consulta a API de licenciamento a partir da identidade do próprio
/// dispositivo (uuid/hostname) — nada é fixado no fonte. O servidor decide,
/// do lado dele, a qual cliente/licença aquele uuid/hostname pertence, e
/// devolve o id_server/relay_server/key compartilhados da infraestrutura.
fn check_license() -> LicenseOutcome {
    use hbb_common::config::LocalConfig;

    let uuid = crate::ui_interface::get_uuid();
    let hostname = crate::common::hostname();

    // build_version/build_hash/build_date identificam exatamente qual build
    // deste fork está rodando no dispositivo — diferente de "version" (que é
    // a versão do RustDesk em si e não muda entre nossos commits), permite
    // ao backend saber quem ainda está numa build antiga sem depender de
    // descoberta manual via chamado de suporte.
    let body = serde_json::json!({
        "uuid": uuid,
        "hostname": hostname,
        "build_version": crate::VERSION,
        "build_hash": crate::GIT_HASH,
        "build_date": crate::BUILD_DATE,
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
                LocalConfig::set_option(LICENSE_LAST_VALIDATED_KEY.to_owned(), "".to_owned());
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
                LocalConfig::set_option(
                    LICENSE_LAST_VALIDATED_KEY.to_owned(),
                    chrono::Utc::now().timestamp().to_string(),
                );
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
    use chrono::TimeZone;
    use hbb_common::config::LocalConfig;

    let blocked = |message: String| LicenseOutcome::Blocked { message };
    let no_history = || {
        blocked(
            "Sem conexão com o servidor de licenciamento e nenhuma validação anterior \
             registrada neste dispositivo. Verifique a internet e tente novamente."
                .to_owned(),
        )
    };

    let cached = LocalConfig::get_option(LICENSE_SERVER_CACHE_KEY);
    let last_validated = LocalConfig::get_option(LICENSE_LAST_VALIDATED_KEY);
    if cached.is_empty() || last_validated.is_empty() {
        return no_history();
    }

    let last_validated_ts: i64 = match last_validated.parse() {
        Ok(ts) => ts,
        Err(_) => return no_history(),
    };
    let elapsed_secs = chrono::Utc::now().timestamp() - last_validated_ts;
    if !(0..=LICENSE_GRACE_PERIOD_SECS).contains(&elapsed_secs) {
        return blocked(
            "Licença não revalidada com o servidor Infomek há mais de 3,5 dias. \
             Verifique a internet ou contate o suporte."
                .to_owned(),
        );
    }

    let mut parts = cached.splitn(3, '\n');
    let id_server = parts.next().unwrap_or_default().to_owned();
    let relay_server = parts.next().unwrap_or_default().to_owned();
    let key = parts.next().unwrap_or_default().to_owned();
    if id_server.is_empty() {
        return no_history();
    }

    let grace_expires_at = chrono::Local
        .timestamp_opt(last_validated_ts, 0)
        .single()
        .unwrap_or_else(chrono::Local::now)
        + chrono::Duration::seconds(LICENSE_GRACE_PERIOD_SECS);

    // Não aparece em tela — fica só no log em disco do app, como pedido.
    hbb_common::log::warn!(
        "Licença operando em modo offline (cache local, sem contato com a API \
         desde {}s atrás). Prazo de tolerância até {}.",
        elapsed_secs,
        grace_expires_at.format("%d/%m/%Y %H:%M")
    );

    LicenseOutcome::CachedGrace {
        server: ServerConfig {
            id_server,
            relay_server,
            key,
        },
        grace_expires_at,
    }
}

/// Mostra um alerta nativo (sem depender do Flutter já estar carregado).
/// Bloqueia até o usuário clicar OK, mas não decide sozinho se o processo
/// deve encerrar depois — quem chama decide isso.
fn native_alert(text: &str) {
    #[cfg(target_os = "windows")]
    {
        crate::platform::windows::message_box(text);
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
                .arg(text)
                .status();
        }
    }
}

/// Diálogo mostrado antes de encerrar o processo por licença negada/bloqueada
/// (o chamador é responsável por sair em seguida). Ao clicar OK, some — não
/// reaparece sozinho: só volta se o processo for relançado de novo (e, com
/// LICENSE_BLOCKED_EXIT_CODE, isso só acontece no próximo restart do serviço).
fn show_license_blocked_dialog(message: &str) {
    let text = format!("{}!", message.trim_end_matches('!'));
    hbb_common::log::error!("{}", text);
    native_alert(&text);
}

/// Diálogo de aviso não-bloqueante: informa que a licença está operando em
/// modo offline (dentro do prazo de tolerância) e some ao clicar OK, sem
/// encerrar o processo. Só reaparece no próximo restart do processo (não
/// fica reabrindo em loop).
fn show_license_grace_notice(message: &str) {
    let text = format!("{}.", message.trim_end_matches('.'));
    hbb_common::log::warn!("{}", text);
    native_alert(&text);
}

/// Aplica id_server/relay_server/key da licença como config "fixa" (sem
/// checkbox editável na UI). Compartilhado entre o caminho online (Valid) e
/// o caminho em modo de tolerância offline (CachedGrace) — mesma config,
/// única diferença é se veio de uma checagem fresca ou do cache local.
fn apply_server_config(server: ServerConfig) {
    let mut overwrite = hbb_common::config::OVERWRITE_SETTINGS.write().unwrap();
    if !server.id_server.is_empty() {
        overwrite.insert("custom-rendezvous-server".to_owned(), server.id_server);
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

pub fn apply() {
    use hbb_common::config;

    // Nome exibido na barra de tarefas/tooltip da bandeja (bind.mainGetAppNameSync()
    // no Flutter, lê hbb_common::config::APP_NAME) — sem isso o instalador/Program
    // Files/Gerenciador de Tarefas mostram "MyRemote", mas o app em si continua
    // dizendo "RustDesk" internamente. Setado o mais cedo possível em apply()
    // (chamado antes de qualquer janela/tray existir) pra não ter nem um piscar
    // do nome antigo. APP_NAME também define a pasta de config local
    // (%APPDATA%\MyRemote\ no Windows) — na primeira execução após o update,
    // cada dispositivo já em campo passa a gerar um ID numérico novo do
    // RustDesk (a pasta antiga fica órfã), mas o uuid usado pela licença vem
    // do machine_uid do Windows, não dessa pasta — o vínculo com o cliente no
    // painel admin não se perde.
    *config::APP_NAME.write().unwrap() = "MyRemote".to_owned();

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
            apply_server_config(server);
            hbb_common::config::Config::set_permanent_password(&daily_password());
            spawn_daily_password_refresher();
        }
        LicenseOutcome::CachedGrace {
            server,
            grace_expires_at,
        } => {
            apply_server_config(server);
            hbb_common::config::Config::set_permanent_password(&daily_password());
            spawn_daily_password_refresher();
            show_license_grace_notice(&format!(
                "Período de teste Infomek ativo (sem conexão com o servidor de \
                 licenciamento) — expira em {}",
                grace_expires_at.format("%d/%m/%Y %H:%M")
            ));
        }
        LicenseOutcome::Denied { message } => {
            show_license_blocked_dialog(&message);
            std::process::exit(LICENSE_BLOCKED_EXIT_CODE);
        }
        LicenseOutcome::Blocked { message } => {
            show_license_blocked_dialog(&message);
            std::process::exit(LICENSE_BLOCKED_EXIT_CODE);
        }
    }
}
