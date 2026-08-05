use librustdesk::*;

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    // Este binário é o LaunchDaemon (roda sem sessão de usuário logada, sem
    // desktop) — marcar antes de load_custom_client() pra que deploy.rs saiba
    // não mostrar diálogo nativo bloqueante (native_alert) se a licença cair
    // em modo offline/negada aqui, mesmo risco de trava do lado Windows.
    crate::common::set_is_service_process();
    crate::common::load_custom_client();
    hbb_common::init_log(false, "service");
    crate::start_os_service();
}
