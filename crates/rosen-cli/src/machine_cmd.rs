//! Machine management and non-interactive enrollment (SPEC R14).
use crate::commands::{MachineCommand, MachineKeyCommand};
use rosen_client::protocol::machine::{CreateJoinKey, MachineRole};
use rosen_client::{Client, Endpoint, Error};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run(json: bool, endpoint: &Endpoint, command: &MachineCommand) -> Result<bool, Error> {
    if matches!(command, MachineCommand::Join { .. }) {
        return join(json, command).await;
    }
    let client = rosen_client::connect_or_start(endpoint).await?;
    match command {
        MachineCommand::List => print(
            json,
            "rosen.machines/v1",
            "machines",
            json!(client.machine_list().await?),
        ),
        MachineCommand::Approve { machine } => print(
            json,
            "rosen.machine/v1",
            "machine",
            json!(client.machine_approve(machine).await?),
        ),
        MachineCommand::Revoke { machine } => print(
            json,
            "rosen.machine/v1",
            "machine",
            json!(client.machine_revoke(machine).await?),
        ),
        MachineCommand::Key(command) => key_command(json, &client, command).await?,
        MachineCommand::Join { .. } => unreachable!(),
    }
    Ok(true)
}

async fn join(json: bool, command: &MachineCommand) -> Result<bool, Error> {
    if let MachineCommand::Join {
        key,
        address,
        fingerprint,
        name,
        data_dir,
    } = command
    {
        let name = name.clone().unwrap_or_else(|| {
            std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "machine".into())
        });
        let credential = match key {
            Some(key) => rosen_client::enrollment::join(data_dir, key, &name).await?,
            None => {
                rosen_client::enrollment::pending(
                    data_dir,
                    address.as_deref().ok_or_else(|| {
                        invalid("join needs a key or --address and --fingerprint")
                    })?,
                    parse_pin(
                        fingerprint
                            .as_deref()
                            .ok_or_else(|| invalid("join needs --fingerprint"))?,
                    )?,
                    &name,
                    MachineRole::Agent,
                )
                .await?
            }
        };
        // Credential secrets stay in encrypted storage, never in command output.
        print(
            json,
            "rosen.machine/v1",
            "machine",
            json!({"id":credential.machine_id}),
        );
        return Ok(true);
    }
    unreachable!()
}

async fn key_command(
    json: bool,
    client: &Client,
    command: &MachineKeyCommand,
) -> Result<(), Error> {
    match command {
        MachineKeyCommand::Create {
            role,
            expires,
            uses,
            address,
        } => {
            let key = create_key(client, role, expires, *uses, address.as_deref()).await?;
            print(json, "rosen.machine_key/v1", "key", json!(key));
        }
        MachineKeyCommand::List => print(
            json,
            "rosen.machine_keys/v1",
            "keys",
            json!(client.join_key_list().await?),
        ),
        MachineKeyCommand::Revoke { key } => print(
            json,
            "rosen.machine_key/v1",
            "key",
            json!(client.join_key_revoke(key).await?),
        ),
    }
    Ok(())
}

async fn create_key(
    client: &Client,
    role: &str,
    expires: &str,
    uses: u32,
    address: Option<&str>,
) -> Result<String, Error> {
    if uses == 0 {
        return Err(invalid("--uses must be positive"));
    }
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("system clock precedes Unix epoch"))?
        .as_secs()
        .checked_add(duration(expires)?)
        .ok_or_else(|| invalid("expiry overflows"))?;
    let address = match address {
        Some(address) => address.to_owned(),
        None => client
            .status()
            .await?
            .listen
            .ok_or_else(|| invalid("server has no network listener"))?,
    };
    let role = if role == "checker" {
        MachineRole::Checker
    } else {
        MachineRole::Agent
    };
    let key = client
        .create_join_key(CreateJoinKey {
            address,
            expires_at,
            use_limit: uses,
            role,
        })
        .await?;
    Ok(key)
}

fn invalid(message: &str) -> Error {
    Error::InvalidArgument(message.into())
}

fn parse_pin(text: &str) -> Result<[u8; 32], Error> {
    let digits = text.replace(':', "");
    if digits.len() != 64 || !digits.is_ascii() {
        return Err(invalid("invalid SHA-256 fingerprint"));
    }
    let mut pin = [0; 32];
    for (i, byte) in pin.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16)
            .map_err(|_| invalid("invalid SHA-256 fingerprint"))?;
    }
    Ok(pin)
}

fn duration(text: &str) -> Result<u64, Error> {
    let (digits, multiplier) = match text.as_bytes().last() {
        Some(b's') => (&text[..text.len() - 1], 1),
        Some(b'm') => (&text[..text.len() - 1], 60),
        Some(b'h') => (&text[..text.len() - 1], 3600),
        Some(b'd') => (&text[..text.len() - 1], 86400),
        _ => {
            return Err(invalid(
                "--expires needs a positive duration such as 30m, 24h or 7d",
            ))
        }
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| invalid("invalid --expires duration"))
}

fn print(as_json: bool, schema: &str, field: &str, value: Value) {
    if as_json {
        println!("{}", json!({"schema":schema, field:value}));
    } else if let Some(text) = value.as_str() {
        println!("{text}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON value")
        );
    }
}
