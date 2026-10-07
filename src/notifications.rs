//! Desktop notifications, awaited without blocking the app.

/// A button on a notification: its identifier, then its label.
pub type Action = (&'static str, &'static str);

/// What the user did with a notification.
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    /// Clicked one of the notification's buttons, by identifier.
    Action(String),
    /// Clicked the notification itself.
    Clicked,
    /// Dismissed it, or wasn't waited on.
    Dismissed,
}

/// Asks for permission to notify. macOS only prompts while undecided, and
/// answers whether notifications are allowed.
#[cfg(target_os = "macos")]
pub async fn request_auth() -> Result<bool, String> {
    mac_usernotifications::request_auth()
        .await
        .map_err(|err| format!("{err:#}"))
}

/// Shows a notification. With an action it resolves once the user acts on
/// it, which may be never; without one, as soon as it's delivered.
///
/// This talks to `UNUserNotificationCenter` through its async API: the
/// blocking wrappers refuse to run off the main thread unless its run loop
/// happens to be idle, which it rarely is right after a GitHub check.
#[cfg(target_os = "macos")]
pub async fn show(summary: &str, body: &str, action: Option<Action>) -> Result<Response, String> {
    let mut notification = mac_usernotifications::Notification::new()
        .title(summary)
        .message(body);
    if let Some((id, label)) = action {
        notification = notification.action(mac_usernotifications::Action::button(id, label));
    }
    let handle = notification
        .send()
        .await
        .map_err(|err| format!("{err:#}"))?;
    // Without buttons the response only resolves by polling until the
    // notification leaves Notification Center, so it isn't waited on.
    if action.is_none() {
        return Ok(Response::Dismissed);
    }
    Ok(match handle.response().await {
        Ok(response) if response.is_default_action() => Response::Clicked,
        Ok(response) if !response.is_dismiss_action() && !response.is_timed_out() => {
            Response::Action(response.action_identifier)
        }
        Ok(_) | Err(_) => Response::Dismissed,
    })
}

#[cfg(not(target_os = "macos"))]
pub async fn show(summary: &str, body: &str, action: Option<Action>) -> Result<Response, String> {
    let (summary, body) = (summary.to_owned(), body.to_owned());
    let (tx, rx) = async_channel::bounded(1);
    // Waiting on the click blocks until the user acts, which may be never,
    // so it gets a thread of its own rather than one of the executor's.
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(&summary).body(&body);
        if let Some((id, label)) = action {
            notification.action(id, label);
        }
        let response = match notification.show() {
            Ok(handle) if action.is_some() => {
                let mut response = Response::Dismissed;
                handle.wait_for_action(|clicked| {
                    response = match clicked {
                        "default" => Response::Clicked,
                        "__closed" => Response::Dismissed,
                        id => Response::Action(id.to_owned()),
                    }
                });
                Ok(response)
            }
            Ok(_) => Ok(Response::Dismissed),
            Err(err) => Err(format!("{err:#}")),
        };
        tx.send_blocking(response).ok();
    });
    rx.recv().await.unwrap_or(Ok(Response::Dismissed))
}
