// 桌面端依赖
#[cfg(desktop)]
mod desktops;
#[cfg(target_os = "macos")]
use common_cmd::hide_title_bar_buttons;
#[cfg(desktop)]
use common_cmd::{
    audio, default_window_icon, get_files_meta, get_window_payload, push_window_payload,
    screenshot, set_badge_count, set_height,
};
#[cfg(target_os = "macos")]
use desktops::app_event;
#[cfg(desktop)]
use desktops::{common_cmd, directory_scanner, init, tray, video_thumbnail::get_video_thumbnail};
#[cfg(desktop)]
use directory_scanner::{cancel_directory_scan, get_directory_usage_info_with_progress};
#[cfg(desktop)]
use init::CustomInit;
use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;
pub mod command;
pub mod configuration;
pub mod error;
pub mod im_reqest_client;
pub mod pojo;
pub mod repository;
mod vo;

use crate::command::room_member_command::{
    cursor_page_room_members, get_room_members, page_room, update_my_room_info,
};
use crate::configuration::get_configuration;
use crate::error::CommonError;
use crate::im_reqest_client::ImRequestClient;
use anyhow::Context;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::ops::Deref;

// 移动端依赖
#[cfg(mobile)]
use init::CustomInit;
#[cfg(mobile)]
use mobiles::init;
#[cfg(mobile)]
mod mobiles;

pub struct AppData {
    db_conn: Arc<DatabaseConnection>,
    request_client: Arc<Mutex<ImRequestClient>>,
    user_info: Arc<Mutex<UserInfo>>,
    cache: Cache<String, String>,
}

use crate::command::contact_command::list_contacts_command;
use crate::command::message_command::{
    check_user_init_and_fetch_messages, page_msg, save_msg, send_msg,
};
use tauri::{Listener, Manager};
use tokio::sync::Mutex;

pub async fn run() {
    #[cfg(desktop)]
    {
        setup_desktop().await.unwrap();
    }
    #[cfg(mobile)]
    {
        setup_mobile();
    }
}

#[cfg(desktop)]
async fn setup_desktop() -> Result<(), CommonError> {
    use crate::command::user_command::{save_user_info, update_user_last_opt_time};

    // 创建一个缓存实例
    let cache: Cache<String, String> = Cache::builder()
        // Time to idle (TTI):  30 minutes
        .time_to_idle(Duration::from_secs(30 * 60))
        // Create the cache.
        .build();
    tauri::Builder::default()
        .init_plugin()
        .init_webwindow_event()
        .init_window_event()
        .setup(move |app| {
            let app_handle = app.handle().clone();
            setup_user_info_listener_early(app.handle().clone());
            tauri::async_runtime::spawn(async move {
                match initialize_app_data(app_handle.clone()).await {
                    Ok((db, client, user_info)) => {
                        // 使用 manage 方法在运行时添加状态
                        app_handle.manage(AppData {
                            db_conn: db.clone(),
                            request_client: client.clone(),
                            user_info: user_info.clone(),
                            cache,
                        });
                        let client_guard = client.lock().await;
                        client_guard.set_app_handle(app_handle.clone());
                        drop(client_guard);
                    }
                    Err(e) => {
                        log::error!("初始化应用数据失败: {}", e);
                    }
                }
            });

            tray::create_tray(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            default_window_icon,
            screenshot,
            audio,
            set_height,
            set_badge_count,
            get_video_thumbnail,
            #[cfg(target_os = "macos")]
            hide_title_bar_buttons,
            save_user_info,
            update_user_last_opt_time,
            page_room,
            get_room_members,
            update_my_room_info,
            cursor_page_room_members,
            list_contacts_command,
            page_msg,
            send_msg,
            save_msg,
            push_window_payload,
            get_window_payload,
            get_files_meta,
            get_directory_usage_info_with_progress,
            cancel_directory_scan
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            #[cfg(target_os = "macos")]
            app_event::handle_app_event(&app_handle, event);
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app_handle, event);
            }
        });
    Ok(())
}

// 异步初始化应用数据
async fn initialize_app_data(
    app_handle: tauri::AppHandle,
) -> Result<(Arc<DatabaseConnection>,Arc<Mutex<ImRequestClient>>,Arc<Mutex<UserInfo>>,),CommonError,> {
    use log::info;
    use migration::{Migrator, MigratorTrait};

    // 加载配置
    let configuration = Arc::new(
        get_configuration(&app_handle)
            .map_err(|e| anyhow::anyhow!("Failed to load configuration: {}", e))?,
    );

    // 初始化数据库连接
    let db = Arc::new(
        configuration
            .database
            .connection_string(&app_handle)
            .await?,
    );

    // 数据库迁移
    match Migrator::up(db.as_ref(), None).await {
        Ok(_) => {
            info!("数据库迁移完成");
        }
        Err(e) => {
            eprintln!("Warning: Database migration failed: {}", e);
        }
    }

    // 创建 HTTP 客户端
    let im_request_client = ImRequestClient::new(configuration.backend.base_url.clone()).await?;

    // 创建用户信息
    let user_info = UserInfo {
        token: Default::default(),
        refresh_token: Default::default(),
        uid: Default::default(),
    };

    let client = Arc::new(Mutex::new(im_request_client));
    let user_info = Arc::new(Mutex::new(user_info));

    Ok((db, client, user_info))
}

// 设置用户信息监听器
fn setup_user_info_listener_early(app_handle: tauri::AppHandle) {
    let app_handle_clone = app_handle.clone();
    app_handle.listen("set_user_info", move |event| {
        let app_handle = app_handle_clone.clone();
        tauri::async_runtime::spawn(async move {
            // 等待AppData状态可用
            if let Some(app_data) = app_handle.try_state::<AppData>() {
                if let Ok(payload) = serde_json::from_str::<UserInfo>(&event.payload()) {
                    let client = app_data.request_client.lock().await;

                    // 更新 client 的 token
                    if let Ok(mut token_guard) = client.token.lock() {
                        *token_guard = Some(payload.token.clone());
                    }

                    if let Ok(mut refresh_token_guard) = client.refresh_token.lock() {
                        *refresh_token_guard = Some(payload.refresh_token.clone());
                    }

                    // 更新用户信息
                    let mut user_info = app_data.user_info.lock().await;
                    user_info.uid = payload.uid.clone();
                    user_info.token = payload.token.clone();
                    user_info.refresh_token = payload.refresh_token.clone();

                    // 检查用户的 is_init 状态并获取消息
                    if let Err(e) = check_user_init_and_fetch_messages(
                        &client,
                        app_data.db_conn.deref(),
                        &payload.uid,
                    )
                    .await
                    {
                        log::error!("检查用户初始化状态并获取消息失败: {}", e);
                    }
                }
            }
        });
    });
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub token: String,
    pub refresh_token: String,
    pub uid: String,
}

pub async fn build_request_client() -> Result<reqwest::Client, CommonError> {
    let client = reqwest::Client::builder()
        .build()
        .with_context(|| "Reqwest client 异常")?;
    Ok(client)
}

#[cfg(mobile)]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
fn setup_mobile() {
    tauri::Builder::default()
        .init_plugin()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
