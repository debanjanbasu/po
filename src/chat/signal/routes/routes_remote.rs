//! Remote peer-route cache keyed by identity: cached-route storage and release,
//! eviction on identity changes, and full clears.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
};

use dioxus::logger::tracing::{debug, info};
use tokio::sync::RwLock;
use veilid_core::RouteId;

use crate::{
    config, helpers::async_utils::now_epoch_millis, storage::background_errors::report_nonfatal,
    veilid::core::get_veilid_api,
};

use super::routes_dht::load_routes_from_dht;

struct CachedRoute {
    route_id: RouteId,
    fetched_at: u64,
    timestamp: u64,
}

type IdentityRouteCache = HashMap<Arc<str>, CachedRoute>;
type RouteCache = HashMap<Arc<str>, IdentityRouteCache>;

static ROUTE_CACHE: LazyLock<RwLock<RouteCache>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Maximum peer-device routes retained across the whole cache.
///
/// Every other eviction here is *lazy*: an identity is pruned only when it is
/// signalled again (`cached_peer_routes`/`resolve_peer_routes`), so an identity
/// that is never resolved again keeps its entry — and the Veilid route it
/// pinned via `import_remote_private_route` — for the life of the process. The
/// ceiling is the peer-device working set the app is already sized for: every
/// watched conversation ([`config::chat::MAX_CONVERSATION_WATCHES`]) with each
/// peer identity announcing its full device list
/// ([`config::route::MAX_ENTRIES`]). A realistic peer set therefore stays fully
/// cached; only accumulation past the app's own configured watch span is
/// evicted.
const ROUTE_CACHE_MAX_ROUTES: usize =
    config::chat::MAX_CONVERSATION_WATCHES * config::route::MAX_ENTRIES;

/// Drops least-recently-fetched routes until at most
/// [`ROUTE_CACHE_MAX_ROUTES`] remain, returning their ids for
/// [`release_route_ids`] (releasing the Veilid-side route, not just the map
/// entry). Callers must hold the write lock.
///
/// `fetched_at` is the right victim key: an entry past
/// `config::route::CACHE_TTL_MS` is already unusable, so evicting it loses
/// nothing a live send could have used.
fn evict_over_capacity(cache: &mut RouteCache) -> Vec<RouteId> {
    let total_routes = |cache: &RouteCache| cache.values().map(HashMap::len).sum::<usize>();
    let excess = total_routes(cache).saturating_sub(ROUTE_CACHE_MAX_ROUTES);
    if excess == 0 {
        return Vec::new();
    }

    // One pass instead of a re-scan per eviction: flatten every cached route
    // with its owner, sort oldest-`fetched_at` first, and drop exactly the
    // `excess` oldest. That is O(n log n) once, versus O(excess * n) for the
    // old min-by-key loop, and it emits one aggregated log line.
    let mut all: Vec<(u64, Arc<str>, Arc<str>, RouteId)> = cache
        .iter()
        .flat_map(|(identity, routes)| {
            routes.iter().map(move |(device, route)| {
                (
                    route.fetched_at,
                    Arc::clone(identity),
                    Arc::clone(device),
                    route.route_id.clone(),
                )
            })
        })
        .collect();
    all.sort_unstable_by_key(|(fetched_at, _, _, _)| *fetched_at);

    let mut evicted = Vec::with_capacity(excess);
    for (_, identity, device, route_id) in all.into_iter().take(excess) {
        if let Some(routes) = cache.get_mut(identity.as_ref())
            && routes.remove(device.as_ref()).is_some()
        {
            evicted.push(route_id);
        }
        if cache.get(identity.as_ref()).is_some_and(HashMap::is_empty) {
            cache.remove(identity.as_ref());
        }
    }
    if !evicted.is_empty() {
        info!(
            "Evicted {} least-recently-fetched remote routes (cache over {ROUTE_CACHE_MAX_ROUTES})",
            evicted.len(),
        );
    }
    evicted
}

async fn release_route_ids(route_ids: Vec<RouteId>) {
    if route_ids.is_empty() {
        return;
    }
    match get_veilid_api().await {
        Ok(api) => {
            for route_id in route_ids {
                if let Err(e) = api.release_private_route(route_id) {
                    report_nonfatal(
                        "chat.signal.routes",
                        "Failed to release evicted private route (may leak until expiry)",
                        e,
                    );
                }
            }
        }
        Err(e) => report_nonfatal(
            "chat.signal.routes",
            "Failed to get Veilid API for route release (routes may leak)",
            e,
        ),
    }
}

pub(super) async fn handle_dead_remote_routes(dead_remote: &[RouteId]) {
    if dead_remote.is_empty() {
        return;
    }

    let dead_remote: HashSet<_> = dead_remote.iter().cloned().collect();
    let mut evicted_route_ids = Vec::new();
    {
        let mut cache = ROUTE_CACHE.write().await;
        for device_cache in cache.values_mut() {
            device_cache.retain(|_, route| {
                let keep = !dead_remote.contains(&route.route_id);
                if !keep {
                    evicted_route_ids.push(route.route_id.clone());
                }
                keep
            });
        }
        cache.retain(|_, device_cache| !device_cache.is_empty());
    }

    let evicted_count = evicted_route_ids.len();
    release_route_ids(evicted_route_ids).await;

    if evicted_count > 0 {
        info!("Evicted {evicted_count} dead remote route(s) from cache");
    }
}

pub async fn evict_routes_for_identity(identity_key: &str) {
    let route_ids = ROUTE_CACHE
        .write()
        .await
        .remove(identity_key)
        .unwrap_or_default()
        .into_values()
        .map(|route| route.route_id)
        .collect();
    release_route_ids(route_ids).await;
}

pub async fn clear_route_cache() {
    let route_ids = ROUTE_CACHE
        .write()
        .await
        .drain()
        .flat_map(|(_, routes)| routes.into_values().map(|route| route.route_id))
        .collect();
    release_route_ids(route_ids).await;
}

/// The single route-freshness predicate. A cached route is usable when the
/// *cache entry* is within `CACHE_TTL_MS` of when we fetched it AND the
/// route blob's own `timestamp` is within `FRESHNESS_MS` of now — the entry
/// may be recently fetched but carry a peer-published stamp that has since
/// gone stale. Every read, retain and eviction in this file goes through
/// this one predicate, so the three former copies cannot drift apart.
const fn cached_route_is_usable(route: &CachedRoute, now: u64) -> bool {
    now.saturating_sub(route.fetched_at) < config::route::CACHE_TTL_MS
        && now.saturating_sub(route.timestamp) <= config::route::FRESHNESS_MS
}

fn usable_routes(
    identity_routes: &IdentityRouteCache,
    exclude_device: Option<&str>,
    now: u64,
) -> Option<Vec<RouteId>> {
    let routes: Vec<RouteId> = identity_routes
        .iter()
        .filter(|(device_id, route)| {
            exclude_device != Some(device_id.as_ref()) && cached_route_is_usable(route, now)
        })
        .map(|(_, route)| route.route_id.clone())
        .collect();
    (!routes.is_empty()).then_some(routes)
}

fn collect_fresh_routes(
    cache: &RouteCache,
    identity_key: &str,
    exclude_device: Option<&str>,
    now: u64,
) -> Result<Option<Vec<RouteId>>, ()> {
    let Some(identity_routes) = cache.get(identity_key) else {
        return Ok(None);
    };
    if identity_routes
        .values()
        .any(|route| !cached_route_is_usable(route, now))
    {
        return Err(());
    }
    Ok(usable_routes(identity_routes, exclude_device, now))
}

async fn cached_peer_routes(
    identity_key: &str,
    exclude_device: Option<&str>,
    now: u64,
) -> Option<Vec<RouteId>> {
    let fresh = collect_fresh_routes(
        &*ROUTE_CACHE.read().await,
        identity_key,
        exclude_device,
        now,
    );
    if let Ok(routes) = fresh {
        return routes;
    }
    let mut evicted_route_ids = Vec::new();
    let mut cache = ROUTE_CACHE.write().await;
    let (routes, remove_identity) = {
        let identity_routes = cache.get_mut(identity_key)?;
        identity_routes.retain(|_, route| {
            let keep = cached_route_is_usable(route, now);
            if !keep {
                evicted_route_ids.push(route.route_id.clone());
            }
            keep
        });
        let routes = usable_routes(identity_routes, exclude_device, now);
        (routes, identity_routes.is_empty())
    };
    if remove_identity {
        cache.remove(identity_key);
    }
    drop(cache);
    release_route_ids(evicted_route_ids).await;
    routes
}

/// Resolve usable peer routes from cache before reading DHT subkey 8.
///
/// `exclude_device` filters results without evicting the excluded cache entry.
pub(super) async fn resolve_peer_routes(
    identity_key: &str,
    exclude_device: Option<&str>,
) -> Vec<RouteId> {
    let now = now_epoch_millis();
    if let Some(routes) = cached_peer_routes(identity_key, exclude_device, now).await {
        return routes;
    }

    let entries = match load_routes_from_dht(identity_key).await {
        Ok(entries) => entries,
        Err(e) => {
            debug!("Route lookup failed for {identity_key}; using usable cache: {e:#}");
            return cached_peer_routes(identity_key, exclude_device, now_epoch_millis())
                .await
                .unwrap_or_default();
        }
    };
    let now = now_epoch_millis();
    if entries.is_empty() {
        evict_routes_for_identity(identity_key).await;
        return Vec::new();
    }

    let Ok(api) = get_veilid_api().await else {
        return cached_peer_routes(identity_key, exclude_device, now_epoch_millis())
            .await
            .unwrap_or_default();
    };

    let mut routes = Vec::with_capacity(entries.len());
    let mut evicted_route_ids = Vec::new();
    let mut cache_guard = ROUTE_CACHE.write().await;
    {
        let cache = &mut *cache_guard;
        let remove_identity = {
            let identity_routes = cache.entry(Arc::from(identity_key)).or_default();
            let mut live_devices = HashSet::with_capacity(entries.len());

            for entry in entries {
                let device_id = entry.device_id;
                live_devices.insert(Arc::clone(&device_id));

                let timestamp = entry.timestamp;
                if let Some(cached) = identity_routes.get_mut(device_id.as_ref())
                    && now.saturating_sub(cached.fetched_at) < config::route::CACHE_TTL_MS
                {
                    cached.timestamp = timestamp;
                    if exclude_device != Some(device_id.as_ref()) {
                        routes.push(cached.route_id.clone());
                    }
                    continue;
                }
                if let Some(stale) = identity_routes.remove(device_id.as_ref()) {
                    evicted_route_ids.push(stale.route_id);
                }
                match api.import_remote_private_route(entry.blob) {
                    Ok(route_id) => {
                        identity_routes.insert(
                            Arc::clone(&device_id),
                            CachedRoute {
                                route_id: route_id.clone(),
                                fetched_at: now,
                                timestamp,
                            },
                        );
                        if exclude_device != Some(device_id.as_ref()) {
                            routes.push(route_id);
                        }
                    }
                    Err(e) => {
                        // The import failure was invisible: the only symptom was
                        // "no routes" much later, with nothing connecting the
                        // two. Name it here.
                        report_nonfatal(
                            "chat.signal.routes",
                            "Failed to import remote private route (stale or corrupt blob)",
                            e,
                        );
                    }
                }
            }

            identity_routes.retain(|device_id, route| {
                let keep = live_devices.contains(device_id) && cached_route_is_usable(route, now);
                if !keep {
                    evicted_route_ids.push(route.route_id.clone());
                }
                keep
            });
            identity_routes.is_empty()
        };
        if remove_identity {
            cache.remove(identity_key);
        }
        // The per-identity retain above only prunes the identity being
        // resolved; identities that are never signalled again are pruned here.
        evicted_route_ids.extend(evict_over_capacity(&mut *cache));
    }
    // Hand the cache back before the async release below: a concurrent
    // resolver must not wait on `release_route_ids`'s Veilid round trips.
    drop(cache_guard);

    release_route_ids(evicted_route_ids).await;
    routes
}
