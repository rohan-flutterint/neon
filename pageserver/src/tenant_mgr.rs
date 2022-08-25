//! This module acts as a switchboard to access different tenants managed by this
//! page server. The code to handle tenant-related mgmt API commands like Attach,
//! Detach or Create tenant is here.

use crate::config::PageServerConf;
use crate::layered_repository::{Repository, TenantState};
use crate::task_mgr;
use crate::task_mgr::TaskKind;
use crate::tenant_config::TenantConfOpt;
use crate::walredo::PostgresRedoManager;
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::*;

use utils::zid::ZTenantId;

static TENANTS: Lazy<RwLock<HashMap<ZTenantId, Arc<Repository>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn read_tenants() -> RwLockReadGuard<'static, HashMap<ZTenantId, Arc<Repository>>> {
    TENANTS
        .read()
        .expect("Failed to read() tenants lock, it got poisoned")
}
fn write_tenants() -> RwLockWriteGuard<'static, HashMap<ZTenantId, Arc<Repository>>> {
    TENANTS
        .write()
        .expect("Failed to write() tenants lock, it got poisoned")
}

///
/// Initialize Repository structs for tenants that are found on local disk. This is
/// called once at pageserver startup.
///
pub fn init_tenant_mgr(conf: &'static PageServerConf) -> anyhow::Result<()> {
    // Scan local filesystem for attached tenants
    let tenants_dir = conf.tenants_path();
    for dir_entry in std::fs::read_dir(&tenants_dir)
        .with_context(|| format!("Failed to list tenants dir {}", tenants_dir.display()))?
    {
        match &dir_entry {
            Ok(dir_entry) => {
                let tenant_id: ZTenantId = dir_entry
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .parse()
                    .unwrap();

                // Start loading the tenant into memory. It will initially be in Loading
                // state.
                let repo = Repository::spawn_load(conf, tenant_id)?;
                write_tenants().insert(tenant_id, repo);
            }
            Err(e) => {
                // On error, print it, but continue with the other tenants. If we error out
                // here, the pageserver startup fails altogether, causing outage for *all*
                // tenants. That seems worse.
                error!(
                    "Failed to list tenants dir entry {:?} in directory {}, reason: {:?}",
                    dir_entry,
                    tenants_dir.display(),
                    e,
                );
            }
        }
    }

    Ok(())
}

///
/// Shut down all tenants. This runs as part of pageserver shutdown.
///
pub async fn shutdown_all_tenants() {
    let tenant_ids = {
        let mut m = write_tenants();
        let mut tenant_ids = Vec::new();
        for (tenantid, tenant) in m.iter_mut() {
            tenant.state.send_modify(|state_guard| match *state_guard {
                TenantState::Loading
                | TenantState::Attaching
                | TenantState::Active
                | TenantState::Stopping => {
                    *state_guard = TenantState::Stopping;
                    tenant_ids.push(*tenantid)
                }
                TenantState::Broken => {}
            });
        }
        tenant_ids
    };

    task_mgr::shutdown_tasks(Some(TaskKind::WalReceiverManager), None, None).await;

    // Ok, no background tasks running anymore. Flush any remaining data in
    // memory to disk.
    //
    // We assume that any incoming connections that might request pages from
    // the repository have already been terminated by the caller, so there
    // should be no more activity in any of the repositories.
    //
    // On error, log it but continue with the shutdown for other tenants.
    for tenant_id in tenant_ids {
        debug!("shutdown tenant {tenant_id}");
        match get_tenant(tenant_id) {
            Ok(repo) => {
                if let Err(err) = repo.checkpoint().await {
                    error!("Could not checkpoint tenant {tenant_id} during shutdown: {err:?}");
                }
            }
            Err(err) => {
                error!("Could not get repository for tenant {tenant_id} during shutdown: {err:?}");
            }
        }
    }
}

pub fn create_tenant(
    conf: &'static PageServerConf,
    tenant_conf: TenantConfOpt,
    tenant_id: ZTenantId,
) -> anyhow::Result<Option<ZTenantId>> {
    match write_tenants().entry(tenant_id) {
        Entry::Occupied(_) => {
            debug!("tenant {tenant_id} already exists");
            Ok(None)
        }
        Entry::Vacant(v) => {
            let wal_redo_manager = Arc::new(PostgresRedoManager::new(conf, tenant_id));
            let repo = Repository::create(conf, tenant_conf, tenant_id, wal_redo_manager)?;
            v.insert(Arc::new(repo));
            Ok(Some(tenant_id))
        }
    }
}

pub fn update_tenant_config(
    tenant_conf: TenantConfOpt,
    tenant_id: ZTenantId,
) -> anyhow::Result<()> {
    info!("configuring tenant {tenant_id}");
    let repo = get_tenant(tenant_id)?;

    repo.update_tenant_config(tenant_conf)?;
    Ok(())
}

///
/// Get reference to a Tenant's Repository object. Note that the tenant
/// can be in any state, including Broken or Loading. If you are going to access
/// the timelines or data in the tenant, you need to ensure that it is in Active
/// state. See use get_active_tenant().
///
pub fn get_tenant(tenant_id: ZTenantId) -> anyhow::Result<Arc<Repository>> {
    let m = read_tenants();
    let tenant = m
        .get(&tenant_id)
        .with_context(|| format!("Tenant {tenant_id} not found"))?;

    Ok(Arc::clone(tenant))
}

///
/// Get reference to a tenant's Repository object. If the tenant is
/// not in active state yet, we will wait for it to become active,
/// with a 30 s timeout. Returns an error if the tenant does not
/// exist, or it's not active yet and the wait times out,
///
pub async fn get_active_tenant(tenant_id: ZTenantId) -> anyhow::Result<Arc<Repository>> {
    let tenant = get_tenant(tenant_id)?;

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tenant.wait_until_active(),
    )
    .await
    {
        Ok(Ok(())) => Ok(tenant),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("timeout waiting for tenant {} to become active", tenant_id),
    }
}

pub async fn detach_tenant(tenant_id: ZTenantId) -> anyhow::Result<()> {
    let repo = get_tenant(tenant_id)?;
    let task = repo.detach_tenant();

    // FIXME: Should we go ahead and remove the tenant anyway, if detaching fails? It's a bit
    // annoying if a tenant gets wedged so that you can't even detach it. OTOH, it's scary
    // to delete files if we're not sure what's wrong.
    match task.await {
        Ok(_) => {
            write_tenants().remove(&tenant_id);
        }
        Err(err) => {
            error!("detaching tenant {} failed: {:?}", tenant_id, err);
        }
    };
    Ok(())
}

///
/// Get list of tenants, for the mgmt API
///
pub fn list_tenants() -> Vec<(ZTenantId, TenantState)> {
    read_tenants()
        .iter()
        .map(|(id, tenant)| (*id, tenant.get_state()))
        .collect()
}

///
/// Execute Attach mgmt API command.
///
/// Downloading all the tenant data is performed in the background,
/// this awn the background task and returns quickly.
///
pub fn attach_tenant(conf: &'static PageServerConf, tenant_id: ZTenantId) -> Result<()> {
    match write_tenants().entry(tenant_id) {
        Entry::Occupied(e) => {
            // Cannot attach a tenant that already exists. The error message depends on
            // the state it's in.
            match e.get().get_state() {
                TenantState::Loading | TenantState::Active => {
                    bail!("tenant {tenant_id} already exists")
                }
                TenantState::Attaching => bail!("tenant {tenant_id} attach is already in progress"),
                TenantState::Stopping => bail!("tenant {tenant_id} is being shut down"),
                TenantState::Broken => bail!("tenant {tenant_id} is marked as broken"),
            }
        }
        Entry::Vacant(v) => {
            let repo = Repository::spawn_attach(conf, tenant_id)?;
            v.insert(repo);
            Ok(())
        }
    }
}
