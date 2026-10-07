//! Desktop notifications: OS acceptance first, user interaction separately.

use std::{future::Future, time::Duration};

/// Observers and Notification Center entries have a finite lifetime, even
/// when a desktop never reports dismissal (including macOS “Clear All”).
pub const RESPONSE_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// Fixed IDs bound macOS's response registry as well as our active tasks.
pub const MAX_ACTIVE: usize = 32;

pub fn free_slot(occupied: impl Fn(usize) -> bool) -> Option<usize> {
    (0..MAX_ACTIVE).find(|&slot| !occupied(slot))
}

/// A button on a notification: its identifier, then its label.
pub type Action = (&'static str, &'static str);

#[derive(Debug, Clone, Default)]
pub enum Permission {
    #[default]
    Checking,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Allowed(String),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Denied,
    Unknown(String),
}

/// Read permission without prompting. Linux notification servers expose no
/// portable per-application permission query, so do not claim authorization.
pub async fn permission() -> Permission {
    #[cfg(target_os = "macos")]
    {
        use mac_usernotifications::{
            AuthorizationStatus as Auth, NotificationSettingStatus as Setting,
        };
        match mac_usernotifications::get_notification_settings().await {
            Ok(settings) => match settings.authorization_status {
                Auth::Denied => Permission::Denied,
                Auth::Authorized | Auth::Provisional | Auth::Ephemeral => Permission::Allowed(
                    if settings.alert_enabled == Setting::Enabled {
                        "Allowed"
                    } else {
                        "Allowed; banners are off or delivered quietly"
                    }
                    .into(),
                ),
                Auth::NotDetermined => {
                    Permission::Unknown("Permission has not been granted yet".into())
                }
                Auth::Unknown => {
                    Permission::Unknown("macOS returned an unknown authorization status".into())
                }
            },
            Err(err) => Permission::Unknown(format!(
                "Cannot read permission: {err}. macOS requires a signed .app bundle."
            )),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permission::Unknown("Permission cannot be queried on this desktop. Use Send test notification and check desktop notification settings.".into())
    }
}

/// What the user did with a notification.
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Action(String),
    Clicked,
    Dismissed,
}

#[cfg(target_os = "macos")]
pub async fn request_auth() -> Result<bool, String> {
    mac_usernotifications::request_auth()
        .await
        .map_err(|err| format!("{err:#}"))
}

#[cfg(any(target_os = "macos", test))]
fn permission_result(allowed: bool) -> Result<(), String> {
    if allowed {
        Ok(())
    } else {
        Err("Notifications are disabled. Enable Allow Notifications for Octowatcher in System Settings → Notifications.".into())
    }
}

pub struct Delivered {
    #[cfg(target_os = "macos")]
    handle: mac_usernotifications::NotificationHandle,
    #[cfg(not(target_os = "macos"))]
    handle: notify_rust::NotificationHandle,
    observe: bool,
}

/// Returns as soon as the notification service accepts the request. This
/// cannot establish that a banner was seen (for example, while in Focus).
#[cfg(target_os = "macos")]
pub async fn send(
    slot: usize,
    summary: &str,
    body: &str,
    action: Option<Action>,
) -> Result<Delivered, String> {
    use mac_usernotifications::AuthorizationStatus;
    let settings = mac_usernotifications::get_notification_settings()
        .await
        .map_err(|err| format!("{err:#}"))?;
    permission_result(matches!(
        settings.authorization_status,
        AuthorizationStatus::Authorized
            | AuthorizationStatus::Provisional
            | AuthorizationStatus::Ephemeral
    ))?;
    let mut notification = mac_usernotifications::Notification::new()
        .id(&format!("octowatcher-{slot}"))
        .title(summary)
        .message(body)
        .timeout(RESPONSE_LIFETIME);
    if let Some((id, label)) = action {
        notification = notification.action(mac_usernotifications::Action::button(id, label));
    }
    let handle = notification
        .send()
        .await
        .map_err(|err| format!("{err:#}"))?;
    Ok(Delivered {
        handle,
        observe: action.is_some(),
    })
}

#[cfg(not(target_os = "macos"))]
pub async fn send(
    _slot: usize,
    summary: &str,
    body: &str,
    action: Option<Action>,
) -> Result<Delivered, String> {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Octowatcher")
        .summary(summary)
        .body(body)
        .timeout(RESPONSE_LIFETIME.as_millis() as i32);
    if let Some((id, label)) = action {
        // Linux notification servers only emit a body-click action if it
        // was explicitly advertised in the request's actions array.
        notification.action("default", "Open").action(id, label);
    }
    let handle = notification
        .show_async()
        .await
        .map_err(|err| format!("{err:#}"))?;
    Ok(Delivered {
        handle,
        observe: action.is_some(),
    })
}

impl Delivered {
    #[cfg(target_os = "macos")]
    pub async fn response<F: Future<Output = ()>>(self, timer: impl Fn(Duration) -> F) -> Response {
        if !self.observe {
            return Response::Dismissed;
        }
        let id = self.handle.notification_id().to_owned();
        let response = futures_lite::future::or(
            async {
                match self.handle.response().await {
                    Ok(response) if response.is_default_action() => Response::Clicked,
                    Ok(response) if !response.is_dismiss_action() && !response.is_timed_out() => {
                        Response::Action(response.action_identifier)
                    }
                    Ok(_) | Err(_) => Response::Dismissed,
                }
            },
            async {
                timer(RESPONSE_LIFETIME).await;
                Response::Dismissed
            },
        )
        .await;
        futures_lite::future::or(
            mac_usernotifications::close_delivered(&id),
            timer(Duration::from_secs(15)),
        )
        .await;
        response
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn response<F: Future<Output = ()>>(self, timer: impl Fn(Duration) -> F) -> Response {
        if !self.observe {
            return Response::Dismissed;
        }
        let mut response = Response::Dismissed;
        // An async wait avoids a permanent OS thread per notification. The
        // app timer also bounds observation if the server ignores its TTL.
        futures_lite::future::or(
            async {
                self.handle
                    .wait_for_action_async(|received| {
                        response = match received {
                            notify_rust::NotificationResponse::Default => Response::Clicked,
                            notify_rust::NotificationResponse::Action(id) => {
                                Response::Action(id.clone())
                            }
                            _ => Response::Dismissed,
                        };
                    })
                    .await;
            },
            timer(RESPONSE_LIFETIME),
        )
        .await;
        // A broken server must not keep a finished observer alive during cleanup.
        futures_lite::future::or(self.handle.close_async(), timer(Duration::from_secs(15))).await;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_ACTIVE, free_slot, permission_result};

    #[test]
    fn active_observers_are_bounded_and_slots_can_be_reused() {
        let mut occupied = std::collections::HashSet::new();
        for _ in 0..MAX_ACTIVE {
            let slot = free_slot(|slot| occupied.contains(&slot)).unwrap();
            assert!(occupied.insert(slot));
        }
        assert_eq!(free_slot(|slot| occupied.contains(&slot)), None);
        occupied.remove(&5);
        assert_eq!(free_slot(|slot| occupied.contains(&slot)), Some(5));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated session bus running tests/notification-service.py"]
    fn linux_notification_service_contract() {
        use super::{RESPONSE_LIFETIME, Response, send};
        use crate::{
            review_notifications::{Delivery, ReviewAction, Target},
            store::{PendingReview, Store},
        };
        futures_lite::future::block_on(async {
            let review = PendingReview {
                host: crate::repository::default_host(),
                repo: "test/repo".into(),
                number: 1,
                title: "Test review".into(),
                url: "https://github.com/test/repo/pull/1".into(),
                author: "someone".into(),
                is_draft: false,
                rereview: false,
                requested_at: None,
            };
            let single = Target::Single(review.clone());
            let summary = Target::Summary;
            let mut store = Store::default();
            let mut delivery = Delivery::default();
            let validated_hosts = [review.host.clone()].into();
            store.reconcile(vec![review.clone()]);
            for failure in ["denied", "failure"] {
                let batch = delivery.begin(&store, &validated_hosts).unwrap();
                let shown = send(0, "Contract test", failure, Some(single.action())).await;
                assert!(shown.is_err());
                delivery.complete(&mut store, &batch, false, &validated_hosts);
                assert_eq!(store.notification_queue.len(), 1);
            }
            let batch = delivery.begin(&store, &validated_hosts).unwrap();
            let shown = send(0, "Contract test", "click:default", Some(single.action()))
                .await
                .unwrap();
            delivery.complete(&mut store, &batch, true, &validated_hosts);
            // A real D-Bus send has completed; no click was needed for acknowledgment.
            store.reconcile(vec![review.clone()]);
            assert!(delivery.begin(&store, &validated_hosts).is_none());
            let response = shown.response(|_| std::future::pending()).await;
            assert_eq!(
                single.respond(response, &store),
                Some(ReviewAction::OpenPr(review.url.clone()))
            );
            for (target, body, expected) in [
                (
                    single.clone(),
                    "click:snooze",
                    Some(ReviewAction::Snooze(review)),
                ),
                (
                    summary.clone(),
                    "click:default",
                    Some(ReviewAction::OpenReviews),
                ),
                (
                    summary.clone(),
                    "click:open-reviews",
                    Some(ReviewAction::OpenReviews),
                ),
                (summary.clone(), "dismiss", None),
            ] {
                let shown = send(0, "Contract test", body, Some(target.action()))
                    .await
                    .unwrap();
                let response = shown.response(|_| std::future::pending()).await;
                assert_eq!(target.respond(response, &store), expected);
            }
            // Server deliberately ignores TTL; app timeout still closes the observer.
            let shown = send(0, "Contract test", "hold", Some(summary.action()))
                .await
                .unwrap();
            let response = shown
                .response(|duration| async move {
                    if duration != RESPONSE_LIFETIME {
                        std::future::pending::<()>().await;
                    }
                })
                .await;
            assert_eq!(response, Response::Dismissed);
        });
    }

    #[test]
    fn denied_permission_is_a_delivery_failure() {
        assert!(permission_result(false).is_err());
        assert_eq!(permission_result(true), Ok(()));
    }
}
