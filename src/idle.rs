use anyhow::{Context, Result};
use zbus::blocking::Connection;

const DESTINATION: &str = "org.freedesktop.ScreenSaver";
const PATH: &str = "/org/freedesktop/ScreenSaver";
const INTERFACE: &str = "org.freedesktop.ScreenSaver";

pub struct IdleInhibitor {
    connection: Connection,
    cookie: u32,
}

impl IdleInhibitor {
    pub fn acquire() -> Result<Self> {
        let connection = Connection::session().context("cannot connect to the session D-Bus")?;
        let reply = connection
            .call_method(
                Some(DESTINATION),
                PATH,
                Some(INTERFACE),
                "Inhibit",
                &("termway", "Remote desktop control session"),
            )
            .context("org.freedesktop.ScreenSaver.Inhibit failed")?;
        let cookie = reply
            .body()
            .deserialize::<u32>()
            .context("invalid idle inhibitor cookie")?;
        Ok(Self { connection, cookie })
    }
}

impl Drop for IdleInhibitor {
    fn drop(&mut self) {
        let _ = self.connection.call_method(
            Some(DESTINATION),
            PATH,
            Some(INTERFACE),
            "UnInhibit",
            &self.cookie,
        );
    }
}
