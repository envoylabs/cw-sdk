#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{to_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response};

use crate::{
    error::ContractError,
    execute,
    msg::{ExecuteMsg, InstantiateMsg, QueryMsg, UpdateTokenMsg},
    query,
};

pub const CONTRACT_NAME: &str = "crates.io:cw-token-factory";
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    execute::init(deps, &msg.owner, msg.token_creation_fee)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::UpdateOwnership(action) => execute::update_ownership(
            deps,
            &env.block,
            &info.sender,
            action,
        ),
        ExecuteMsg::UpdateFee {
            token_creation_fee,
        } => execute::update_fee(deps, info, token_creation_fee),
        ExecuteMsg::WithdrawFee {
            to,
        } => execute::withdraw_fee(deps, env, info, to),
        ExecuteMsg::CreateToken {
            nonce,
            admin,
            after_transfer_hook,
        } => execute::create_token(deps, info, nonce, admin, after_transfer_hook),
        ExecuteMsg::UpdateToken(UpdateTokenMsg {
            denom,
            admin,
            after_transfer_hook,
        }) => execute::update_token(deps, info, denom, admin, after_transfer_hook),
        ExecuteMsg::Mint {
            to,
            denom,
            amount,
        } => execute::mint(deps, info, to, denom, amount),
        ExecuteMsg::Burn {
            from,
            denom,
            amount,
        } => execute::burn(deps, info, from, denom, amount),
        ExecuteMsg::ForceTransfer {
            from,
            to,
            denom,
            amount,
        } => execute::force_transfer(deps, info, from, to, denom, amount),
        ExecuteMsg::AfterTransfer {
            from,
            to,
            denom,
            amount,
        } => execute::after_transfer(deps, info, from, to, denom, amount),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    match msg {
        QueryMsg::Ownership {} => to_binary(&cw_ownable::get_ownership(deps.storage)?),
        QueryMsg::TokenCreationFee {} => to_binary(&query::token_creation_fee(deps)?),
        QueryMsg::Token {
            denom,
        } => to_binary(&query::token(deps, denom)?),
        QueryMsg::Tokens {
            start_after,
            limit,
        } => to_binary(&query::tokens(deps, start_after, limit)?),
    }
    .map_err(ContractError::from)
}
