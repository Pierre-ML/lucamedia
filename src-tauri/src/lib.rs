mod db;
mod detect;
mod hls;
mod model;
mod queue;
mod settings;
mod tools;
mod updater;
mod verify;
mod ytdlp;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            queue::init(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            queue::get_settings,
            queue::save_settings,
            queue::queue_state,
            queue::queue_add,
            queue::queue_remove,
            queue::queue_retry,
            queue::queue_cancel,
            queue::queue_clear_finished,
            queue::queue_set_paused,
            queue::verify_again,
            queue::reveal_download,
            queue::open_dest_dir,
            queue::tools_status,
            queue::logs_recent,
            updater::app_check_update,
            updater::app_install_update,
            updater::app_uninstall,
        ])
        .run(tauri::generate_context!())
        .expect("erreur au lancement de l'application Tauri");
}
