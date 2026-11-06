//! Everything Octowatcher does except draw it: finding local clones, syncing
//! review requests through `gh`, persisting state, deciding what to notify,
//! and the platform integrations (notifications, login items, single instance,
//! self-update). No UI toolkit is linked here.

pub mod appearance;
pub mod discovery;
pub mod engine;
pub mod github;
pub mod health;
pub mod instance;
pub mod login;
pub mod notifications;
pub mod refresh_queue;
pub mod repository;
pub mod review_filter;
pub mod review_notifications;
pub mod store;
pub mod updater;
