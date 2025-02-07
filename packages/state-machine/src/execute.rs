use cosmwasm_std::{
    to_binary, Addr, Binary, BlockInfo, ContractInfo, ContractResult, Env, Event, MessageInfo,
    Response, Storage, TransactionInfo,
};
use cosmwasm_vm::{call_execute, call_instantiate, call_sudo, Backend, Instance, InstanceOptions};
use cw_sdk::{address, bank, hash::sha256, Account};
use cw_store::Cached;
use tracing::{debug, info};

use crate::{
    backend::{BackendApi, BackendQuerier, ContractSubstore},
    error::{Error, Result},
    state::{code_by_address, ACCOUNTS, CODES, CODE_COUNT},
};

pub fn store_code(
    store: &mut dyn Storage,
    sender_addr: &Addr,
    wasm_byte_code: &Binary,
) -> Result<Event> {
    // increment the code count
    let code_id = CODE_COUNT.update(store, |count| -> Result<_> {
        Ok(count + 1)
    })?;

    // save code to the store
    CODES.save(store, code_id, wasm_byte_code)?;

    let code_hash = hex::encode(sha256(wasm_byte_code));

    info!(target: "Stored code", id = code_id, hash = code_hash);

    Ok(Event::new("store_code")
        .add_attribute("sender", sender_addr)
        .add_attribute("code_id", code_id.to_string())
        .add_attribute("code_hash", code_hash))
}

#[allow(clippy::too_many_arguments)]
pub fn instantiate_contract(
    store: impl Storage + 'static,
    block: BlockInfo,
    transaction: Option<TransactionInfo>,
    info: &MessageInfo,
    code_id: u64,
    msg: &[u8],
    label: String,
    admin: Option<Addr>,
) -> Result<ContractResult<Response>> {
    let cache = Cached::new(store);

    // validate the label
    //
    // the label must not start with the prefix `cw1`, so that it is not
    // confused with contract addresses
    //
    // we also want to ensure uniqueness: this is done later when updating the
    // Accounts map: if two contracts share the same label, they must also have
    // the same address, which will result in a Error::AccountFound.
    if label.starts_with(&format!("{}1", address::ADDRESS_PREFIX)) {
        return Err(Error::IllegalLabel);
    }

    // now we know the label is valid, derive contract address from it
    let contract_addr = address::derive_from_label(&label)?;

    let env = Env {
        block,
        transaction,
        contract: ContractInfo {
            // TODO: recycle the address later instead of cloning
            address: contract_addr.clone(),
        },
    };

    // load wasm binary code
    let code = CODES.load(&cache, code_id)?;

    // create the wasm instance and call the instantiate entry point
    let mut instance = Instance::from_code(
        &code,
        Backend {
            api: BackendApi,
            storage: ContractSubstore::new(cache, &contract_addr),
            querier: BackendQuerier,
        },
        InstanceOptions {
            gas_limit: u64::MAX,
            print_debug: true,
        },
        None,
    )?;
    let result = call_instantiate(&mut instance, &env, info, msg)?;

    // contract execution is finished; we recycle the cached store
    let mut cache = instance
        .recycle()
        .expect("[cw-state-machine]: failed to recycle instance")
        .storage
        .recycle();

    // if the contract execution is successful, we flush the state changes
    // occurred during the instantiation call to the underlying store, and save
    // the contract account.
    match &result {
        ContractResult::Ok(_) => {
            cache.flush();
            let mut store = cache.recycle();

            ACCOUNTS.update(&mut store, &contract_addr, |opt| {
                // IMPORTANT: NOTE: do not save the account if one of the same
                // address already exists.
                if opt.is_some() {
                    return Err(Error::account_found(&contract_addr));
                }
                Ok(Account::Contract {
                    code_id,
                    label: label.clone(),
                    admin,
                })
            })?;

            info!(
                target: "Instantiated contract",
                address = contract_addr.to_string(),
                code_id,
                label,
            );
        },
        ContractResult::Err(err) => {
            debug!(target: "Failed to instantiate contract", code_id, label, reason = err);
        }
    }

    Ok(result)
}

pub fn sudo_contract<S>(
    store: S,
    env: &Env,
    msg: &[u8],
) -> Result<(ContractResult<Response>, S)>
where
    S: Storage + 'static,
{
    let cache = Cached::new(store);

    // load wasm binary code
    let code = code_by_address(&cache, &env.contract.address)?;

    // create the wasm instance and call the execute entry point
    let mut instance = Instance::from_code(
        &code,
        Backend {
            api: BackendApi,
            storage: ContractSubstore::new(cache, &env.contract.address),
            querier: BackendQuerier,
        },
        InstanceOptions {
            gas_limit: u64::MAX,
            print_debug: true,
        },
        None,
    )?;
    let result = call_sudo(&mut instance, env, msg)?;

    // contract execution is finished; we recycle the cached store
    let mut cache = instance
        .recycle()
        .expect("[cw-state-machine]: failed to recycle instance")
        .storage
        .recycle();

    // if the execution is successful, flush the state changes to the underlying store
    match &result {
        ContractResult::Ok(_) => {
            cache.flush();
            debug!(
                target: "Sudoed contract",
                address = env.contract.address.to_string(),
            );
        },
        ContractResult::Err(err) => {
            debug!(
                target: "Failed to sudo contract",
                address = env.contract.address.to_string(),
                reason = err,
            );
        }
    }

    Ok((result, cache.recycle()))
}

pub fn execute_contract(
    store: impl Storage + 'static,
    env: &Env,
    info: &MessageInfo,
    msg: &[u8],
) -> Result<ContractResult<Response>> {
    let cache = Cached::new(store);

    // if the message has coins attached to it, we first invoke bank contract to
    // transfer the coins
    let (mut fund_events, cache) = if !info.funds.is_empty() {
        transfer_funds(cache, env, info)?
    } else {
        (vec![], cache)
    };

    // load wasm binary code
    let code = code_by_address(&cache, &env.contract.address)?;

    // create the wasm instance and call the execute entry point
    let mut instance = Instance::from_code(
        &code,
        Backend {
            api: BackendApi,
            storage: ContractSubstore::new(cache, &env.contract.address),
            querier: BackendQuerier,
        },
        InstanceOptions {
            gas_limit: u64::MAX,
            print_debug: true,
        },
        None,
    )?;
    let mut result = call_execute(&mut instance, env, info, msg)?;

    // contract execution is finished; we recycle the cached store
    let mut cache = instance
        .recycle()
        .expect("[cw-state-machine]: failed to recycle instance")
        .storage
        .recycle();

    match &mut result {
        ContractResult::Ok(resp) => {
            // flush the state changes
            cache.flush();

            // prepend fund transfer events
            fund_events.extend(resp.events.iter().cloned());
            resp.events = fund_events;

            debug!(
                target: "Executed contract",
                address = env.contract.address.to_string(),
                sender = info.sender.to_string(),
            );
        },
        ContractResult::Err(err) => {
            debug!(
                target: "Failed to execute contract",
                address = env.contract.address.to_string(),
                sender = info.sender.to_string(),
                reason = err,
            );
        }
    }

    Ok(result)
}

pub fn migrate_contract(
    _store: impl Storage + 'static,
    _env: &Env,
    _code_id: u64,
    _msg: &[u8]
) -> Result<ContractResult<Response>> {
    todo!();
}

fn transfer_funds<S>(store: S, env: &Env, info: &MessageInfo) -> Result<(Vec<Event>, S)>
where
    S: Storage + 'static,
{
    let sudo_env = Env {
        block: env.block.clone(),
        transaction: None,
        contract: ContractInfo {
            address: address::derive_from_label("bank")?,
        },
    };

    let sudo_msg = to_binary(&bank::SudoMsg::Transfer {
        from: info.sender.to_string(),
        to: env.contract.address.to_string(),
        coins: info.funds.clone(),
    })?;

    let (result, store) = sudo_contract(store, &sudo_env, &sudo_msg)?;

    match result {
        ContractResult::Ok(resp) => Ok((resp.events, store)),
        ContractResult::Err(err) => Err(Error::fund_transfer_failed(err)),
    }
}
