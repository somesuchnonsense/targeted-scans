use crate::manager::PulseManager;
use crate::runner::PulseRunner;
use crate::settings::Settings;
use autopulse_database::conn::{get_conn, get_pool};
use autopulse_database::models::{FoundStatus, NewScanEvent, ProcessStatus};

#[tokio::test]
async fn confirmed_missing_event_processes_when_check_path_is_enabled() {
    let database_url = "sqlite://:memory:".to_string();
    let mut settings = Settings::default();
    settings.app.database_url = database_url.clone();
    settings.opts.check_path = true;
    let pool = get_pool(&database_url).unwrap();
    get_conn(&pool).unwrap().migrate().unwrap();
    let manager = PulseManager::new(settings, pool);

    let inserted = manager
        .add_event(&NewScanEvent {
            file_path: "/media/deleted.mkv".to_string(),
            found_status: FoundStatus::Missing.into(),
            ..Default::default()
        })
        .unwrap();

    PulseRunner::new(&manager)
        .update_process_status()
        .await
        .unwrap();

    let processed = manager.get_event(&inserted.id).unwrap().unwrap();
    assert_eq!(
        processed.process_status,
        String::from(ProcessStatus::Complete)
    );
}
