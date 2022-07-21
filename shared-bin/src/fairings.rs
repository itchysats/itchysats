use rocket::fairing::AdHoc;
use rocket::fairing::Fairing;

/// Attach this fairing to enable logging Rocket launch
pub fn log_launch() -> impl Fairing {
    AdHoc::on_liftoff("Log launch", |rocket| {
        Box::pin(async move {
            let http_endpoint = format!(
                "http://{}:{}",
                rocket.config().address,
                rocket.config().port
            );

            tracing::info!(target: "http", endpoint = %http_endpoint, "HTTP interface is ready");
        })
    })
}

/// Attach this fairing to enable logging Rocket HTTP requests
pub fn log_requests() -> impl Fairing {
    AdHoc::on_response("Log status code for request", |request, response| {
        Box::pin(async move {
            let method = request.method();
            let path = request.uri().path();
            let status = response.status();

            tracing::debug!(target: "http", %method, %path, %status, "Handled request");
        })
    })
}

/// Attach this fairing to enable loading the UI in the system default browser
pub fn ui_browser_launch() -> impl Fairing {
    AdHoc::on_liftoff("ui browser launch", move |rocket| {
        Box::pin(async move {
            let (username, password) = match (
                rocket.state::<rocket_basicauth::Username>(),
                rocket.state::<rocket_basicauth::Password>(),
            ) {
                (Some(username), Some(password)) => (username, password),
                _ => {
                    tracing::warn!("Username and password not configured correctly");
                    return;
                }
            };

            let http_endpoint = format!(
                "http://{}:{}@{}:{}",
                username,
                password,
                rocket.config().address,
                rocket.config().port
            );

            match webbrowser::open(http_endpoint.as_str()) {
                Ok(()) => {
                    tracing::info!("The user interface was opened in your default browser");
                }
                Err(e) => {
                    tracing::debug!(
                        "Could not open user interface at {} in default browser because {e:#}",
                        http_endpoint
                    );
                }
            }
        })
    })
}
