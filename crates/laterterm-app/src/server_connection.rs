//! 后台 SSH 连接与凭据生命周期；UI 仅处理提示结果，不接触阻塞密钥环调用。

use crate::workspace::run_secret_op;
use anyhow::{Result, bail};
use laterterm_config::{ServerAuthKind, ServerEntry};
use laterterm_core::TerminalSession;
use laterterm_secrets::{SecretStore, ssh_account, ssh_passphrase_account};
use laterterm_ssh::{
    AuthMethod, PrivateKeyPassphraseRequired, SshOptions, SshSession, prepare_private_key,
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialKind {
    Login,
    PrivateKey {
        key_path: PathBuf,
        context: Option<Arc<PrivateKeyContext>>,
    },
}

/// 默认候选解锁上下文，不保存口令；Debug 中的 AuthMethod 自带密钥掩码。
#[derive(Clone, Debug)]
pub(crate) struct PrivateKeyContext {
    prepared: Vec<AuthMethod>,
    remaining: Vec<PathBuf>,
}

impl PartialEq for PrivateKeyContext {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for PrivateKeyContext {}

pub(crate) enum ConnectionOutcome {
    Connected {
        session: Arc<dyn TerminalSession>,
        warning: Option<String>,
    },
    Prompt {
        kind: CredentialKind,
        warning: Option<String>,
    },
}

/// 阻塞入口，仅允许后台线程调用；成功连接后才记住本次输入的凭据。
pub(crate) fn connect(
    entry: &ServerEntry,
    supplied: Option<(CredentialKind, String)>,
    remember: bool,
    store: Arc<dyn SecretStore>,
    active: Arc<AtomicBool>,
) -> Result<ConnectionOutcome> {
    connect_with(
        entry,
        supplied,
        remember,
        store,
        active,
        prepare_private_key,
        |auth| {
            let options = SshOptions::new(&entry.host, entry.port, &entry.username, auth);
            Ok(Arc::new(SshSession::connect(options)?) as Arc<dyn TerminalSession>)
        },
    )
}

fn get_secret(store: &Arc<dyn SecretStore>, account: String) -> Result<Option<String>> {
    let store = Arc::clone(store);
    run_secret_op(move || store.get(&account))
}

fn prompt(kind: CredentialKind, warning: Option<&str>) -> ConnectionOutcome {
    ConnectionOutcome::Prompt {
        kind,
        warning: warning.map(str::to_owned),
    }
}

/// 注入准备器和连接器使测试不读取真实私钥、不访问网络。
fn connect_with<P, C>(
    entry: &ServerEntry,
    supplied: Option<(CredentialKind, String)>,
    remember: bool,
    store: Arc<dyn SecretStore>,
    active: Arc<AtomicBool>,
    prepare: P,
    establish: C,
) -> Result<ConnectionOutcome>
where
    P: Fn(Option<&Path>, Option<&str>) -> std::io::Result<AuthMethod>,
    C: FnOnce(AuthMethod) -> Result<Arc<dyn TerminalSession>>,
{
    anyhow::ensure!(active.load(Ordering::Acquire), "连接已取消");
    let mut to_remember = None;
    let auth = match supplied {
        Some((CredentialKind::PrivateKey { key_path, context }, secret)) => {
            match prepare(Some(&key_path), Some(&secret)) {
                Ok(auth) => {
                    to_remember = Some((ssh_passphrase_account(&entry.name)?, secret));
                    if let Some(context) = context {
                        let mut prepared = context.prepared.clone();
                        prepared.push(auth);
                        match prepare_candidates(
                            entry,
                            &store,
                            &prepare,
                            context.remaining.clone(),
                            prepared,
                        )? {
                            Ok(auth) => auth,
                            Err(outcome) => return Ok(outcome),
                        }
                    } else {
                        auth
                    }
                }
                Err(error) => {
                    if let Some(required) = passphrase_required(&error) {
                        return Ok(prompt(
                            CredentialKind::PrivateKey {
                                key_path: required,
                                context,
                            },
                            Some("私钥口令不正确，请重试"),
                        ));
                    }
                    return Err(error.into());
                }
            }
        }
        Some((CredentialKind::Login, secret)) if !secret.is_empty() => {
            let auth = login_auth(entry.auth, &secret);
            to_remember = Some((ssh_account(&entry.name)?, secret));
            auth
        }
        // 空字符串显式选择私钥；全空白但非空的字符串仍是原样密码。
        Some((CredentialKind::Login, _)) => match prepare_key_auth(entry, &store, &prepare)? {
            Ok(auth) => auth,
            Err(outcome) => return Ok(outcome),
        },
        None if entry.auth == ServerAuthKind::PrivateKey => {
            match prepare_key_auth(entry, &store, &prepare)? {
                Ok(auth) => auth,
                Err(outcome) => return Ok(outcome),
            }
        }
        None => match get_secret(&store, ssh_account(&entry.name)?) {
            Ok(Some(secret)) if !secret.is_empty() => login_auth(entry.auth, &secret),
            Ok(_) => return Ok(prompt(CredentialKind::Login, None)),
            Err(_) => {
                return Ok(prompt(
                    CredentialKind::Login,
                    Some("无法读取密钥环，请手动输入登录凭据"),
                ));
            }
        },
    };
    anyhow::ensure!(active.load(Ordering::Acquire), "连接已取消");
    let session = establish(auth)?;
    if !active.load(Ordering::Acquire) {
        session.kill();
        bail!("连接已取消");
    }
    let mut warning = None;
    if remember && let Some((account, secret)) = to_remember {
        let store = Arc::clone(&store);
        let active = Arc::clone(&active);
        // 在实际执行密钥环操作的线程再检查一次取消标记。
        if run_secret_op(move || {
            if active.load(Ordering::Acquire) {
                store.set(&account, &secret)?;
            }
            Ok(())
        })
        .is_err()
        {
            warning = Some("连接成功，但凭据未能保存到密钥环".into());
        }
    }
    if !active.load(Ordering::Acquire) {
        session.kill();
        bail!("连接已取消");
    }
    Ok(ConnectionOutcome::Connected { session, warning })
}

fn login_auth(kind: ServerAuthKind, secret: &str) -> AuthMethod {
    if kind == ServerAuthKind::KeyboardInteractive {
        AuthMethod::KeyboardInteractive {
            secret: secret.into(),
        }
    } else {
        AuthMethod::Password {
            password: secret.into(),
        }
    }
}

fn passphrase_required(error: &std::io::Error) -> Option<PathBuf> {
    error
        .get_ref()?
        .downcast_ref::<PrivateKeyPassphraseRequired>()
        .map(|required| required.key_path.clone())
}

fn prepare_key_auth<P>(
    entry: &ServerEntry,
    store: &Arc<dyn SecretStore>,
    prepare: &P,
) -> Result<std::result::Result<AuthMethod, ConnectionOutcome>>
where
    P: Fn(Option<&Path>, Option<&str>) -> std::io::Result<AuthMethod>,
{
    let path = entry
        .key_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(Path::new);
    if path.is_none() {
        let home = std::env::home_dir().ok_or_else(|| anyhow::anyhow!("无法确定用户主目录"))?;
        return prepare_candidates(
            entry,
            store,
            prepare,
            vec![home.join(".ssh/id_ed25519"), home.join(".ssh/id_rsa")],
            vec![],
        );
    }
    let required_path = match prepare(path, None) {
        Ok(auth) => return Ok(Ok(auth)),
        Err(error) => match passphrase_required(&error) {
            Some(path) => path,
            None => return Err(error.into()),
        },
    };
    unlock_key(entry, store, prepare, required_path, None)
}

fn prepare_candidates<P>(
    entry: &ServerEntry,
    store: &Arc<dyn SecretStore>,
    prepare: &P,
    paths: Vec<PathBuf>,
    mut prepared: Vec<AuthMethod>,
) -> Result<std::result::Result<AuthMethod, ConnectionOutcome>>
where
    P: Fn(Option<&Path>, Option<&str>) -> std::io::Result<AuthMethod>,
{
    for (index, path) in paths.iter().enumerate() {
        match prepare(Some(path), None) {
            Ok(auth) => prepared.push(auth),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                let Some(required) = passphrase_required(&error) else {
                    return Err(error.into());
                };
                let context = Arc::new(PrivateKeyContext {
                    prepared: prepared.clone(),
                    remaining: paths[index + 1..].to_vec(),
                });
                match unlock_key(entry, store, prepare, required, Some(context))? {
                    Ok(auth) => prepared.push(auth),
                    Err(outcome) => return Ok(Err(outcome)),
                }
            }
        }
    }
    anyhow::ensure!(!prepared.is_empty(), "未找到默认 SSH 私钥");
    let mut keys = Vec::new();
    for auth in prepared {
        if let AuthMethod::PreparedPrivateKey { keys: loaded } = auth {
            keys.extend(loaded);
        }
    }
    Ok(Ok(AuthMethod::PreparedPrivateKey { keys }))
}

fn unlock_key<P>(
    entry: &ServerEntry,
    store: &Arc<dyn SecretStore>,
    prepare: &P,
    required_path: PathBuf,
    context: Option<Arc<PrivateKeyContext>>,
) -> Result<std::result::Result<AuthMethod, ConnectionOutcome>>
where
    P: Fn(Option<&Path>, Option<&str>) -> std::io::Result<AuthMethod>,
{
    let kind = CredentialKind::PrivateKey {
        key_path: required_path.clone(),
        context,
    };
    let mut accounts = vec![ssh_passphrase_account(&entry.name)?];
    // 仅旧 private-key 元数据允许把 ssh 槽位视作遗留解密口令。
    if entry.auth == ServerAuthKind::PrivateKey {
        accounts.push(ssh_account(&entry.name)?);
    }
    let mut failed = false;
    for account in accounts {
        match get_secret(store, account) {
            Ok(Some(secret)) => match prepare(Some(&required_path), Some(&secret)) {
                Ok(auth) => return Ok(Ok(auth)),
                Err(error) if passphrase_required(&error).is_some() => failed = true,
                Err(error) => return Err(error.into()),
            },
            Ok(None) => {}
            Err(_) => {
                return Ok(Err(prompt(
                    kind,
                    Some("无法读取密钥环，请手动输入私钥口令"),
                )));
            }
        }
    }
    Ok(Err(prompt(
        kind,
        if failed {
            Some("已保存的私钥口令不正确，请重新输入")
        } else {
            None
        },
    )))
}

/// 写入/复制凭据；重命名旧槽留待 metadata 保存成功后清理。
/// 唯一例外是原地从私钥切换登录认证：先迁移 legacy 口令，再删除登录槽，
/// 宁可 metadata 保存失败后需要重输口令，也绝不向远端发送私钥口令。
/// active 无法撤销已经进入 Secret Service 的写入；每次变更之前均重新检查。
pub(crate) fn update_server_secrets(
    store: Arc<dyn SecretStore>,
    original: Option<&ServerEntry>,
    meta: &ServerEntry,
    secret: &str,
    active: Arc<AtomicBool>,
) -> Result<()> {
    let renamed = original.is_some_and(|old| old.name != meta.name);
    let switching = original.is_some_and(|old| old.auth == ServerAuthKind::PrivateKey)
        && meta.auth != ServerAuthKind::PrivateKey;
    if secret.is_empty() && !renamed && !switching {
        return Ok(());
    }
    let new_login = ssh_account(&meta.name)?;
    let new_passphrase = ssh_passphrase_account(&meta.name)?;
    let target = if meta.auth == ServerAuthKind::PrivateKey {
        new_passphrase.clone()
    } else {
        new_login.clone()
    };
    let old_accounts = original
        .map(|old| -> Result<_> {
            Ok((
                ssh_account(&old.name)?,
                ssh_passphrase_account(&old.name)?,
                old.auth,
            ))
        })
        .transpose()?;
    let secret = secret.to_owned();
    run_secret_op(move || {
        let check_active = || -> Result<()> {
            anyhow::ensure!(active.load(Ordering::Acquire), "保存已取消");
            Ok(())
        };
        check_active()?;
        if let Some((old_login, old_passphrase, old_auth)) = old_accounts {
            let login = store.get(&old_login)?;
            let passphrase = store.get(&old_passphrase)?;
            let legacy = old_auth == ServerAuthKind::PrivateKey;
            // legacy ssh 仅是解密口令，绝不作为可复制的登录密码。
            let migrated_passphrase =
                passphrase.or_else(|| if legacy { login.clone() } else { None });
            if switching && !renamed {
                if let Some(value) = &migrated_passphrase
                    && store.get(&new_passphrase)?.is_none()
                {
                    check_active()?;
                    store.set(&new_passphrase, value)?;
                }
                check_active()?;
                store.delete(&old_login)?;
            }
            if renamed {
                let login = if legacy { None } else { login };
                for (account, value) in [(new_login, login), (new_passphrase, migrated_passphrase)]
                {
                    if !secret.is_empty() && account == target {
                        continue;
                    }
                    if let Some(value) = value
                        && store.get(&account)?.is_none()
                    {
                        check_active()?;
                        store.set(&account, &value)?;
                    }
                }
            }
        }
        if !secret.is_empty() {
            check_active()?;
            store.set(&target, &secret)?;
        }
        check_active()
    })
}

/// 后台调用，幂等删除两个命名空间；即使一个删除失败也尝试另一个。
pub(crate) fn delete_server_secrets(store: &dyn SecretStore, name: &str) -> Result<()> {
    let login = ssh_account(name)?;
    let passphrase = ssh_passphrase_account(name)?;
    let first = store.delete(&login);
    let second = store.delete(&passphrase);
    first.and(second)
}

/// 仅在元数据已成功持久化后，由主线程安排后台清理。
pub(crate) fn cleanup_renamed_server_secrets(
    store: &dyn SecretStore,
    original: &ServerEntry,
    meta: &ServerEntry,
) -> Result<()> {
    if original.name == meta.name {
        return Ok(());
    }
    delete_server_secrets(store, &original.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use laterterm_core::{BackendKind, SessionStreams};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryStore {
        values: Mutex<HashMap<String, String>>,
        calls: Mutex<Vec<String>>,
        fail: AtomicBool,
        cancel_after_set: Mutex<Option<Arc<AtomicBool>>>,
    }
    impl SecretStore for MemoryStore {
        fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
            self.calls.lock().unwrap().push(format!("get:{account}"));
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "sensitive-store-error");
            Ok(self.values.lock().unwrap().get(account).cloned())
        }
        fn set(&self, account: &str, value: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("set:{account}"));
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "sensitive-store-error");
            self.values
                .lock()
                .unwrap()
                .insert(account.into(), value.into());
            if let Some(active) = self.cancel_after_set.lock().unwrap().take() {
                active.store(false, Ordering::SeqCst);
            }
            Ok(())
        }
        fn delete(&self, account: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("delete:{account}"));
            self.values.lock().unwrap().remove(account);
            Ok(())
        }
    }
    #[derive(Default)]
    struct Session(AtomicBool);
    impl TerminalSession for Session {
        fn backend(&self) -> BackendKind {
            BackendKind::Ssh
        }
        fn title(&self) -> String {
            "test".into()
        }
        fn take_streams(&self) -> std::io::Result<SessionStreams> {
            unreachable!()
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
        fn kill(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    fn entry(auth: ServerAuthKind) -> ServerEntry {
        ServerEntry {
            name: "test".into(),
            host: "invalid.test".into(),
            username: "test".into(),
            auth,
            key_path: Some("/fake/key".into()),
            ..Default::default()
        }
    }
    fn active() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }
    fn prepared() -> AuthMethod {
        AuthMethod::PreparedPrivateKey { keys: vec![] }
    }
    fn encrypted(path: &Path, password: Option<&str>) -> std::io::Result<AuthMethod> {
        if password == Some(" correct ") {
            Ok(prepared())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                PrivateKeyPassphraseRequired {
                    key_path: path.into(),
                },
            ))
        }
    }

    #[test]
    fn login_missing_and_keyring_failure_prompt_without_leaking_errors() {
        for failure in [false, true] {
            let store = Arc::new(MemoryStore::default());
            store.fail.store(failure, Ordering::SeqCst);
            let outcome = connect_with(
                &entry(ServerAuthKind::Password),
                None,
                false,
                store,
                active(),
                |_, _| panic!("不读私钥"),
                |_| panic!("不连接"),
            )
            .unwrap();
            let ConnectionOutcome::Prompt { kind, warning } = outcome else {
                panic!("应提示")
            };
            assert_eq!(kind, CredentialKind::Login);
            assert_eq!(warning.is_some(), failure);
            assert!(!format!("{warning:?}").contains("sensitive-store-error"));
        }
    }

    #[test]
    fn unencrypted_private_key_never_reads_keyring() {
        let store = Arc::new(MemoryStore::default());
        let result = connect_with(
            &entry(ServerAuthKind::PrivateKey),
            None,
            false,
            store.clone(),
            active(),
            |path, pass| {
                assert_eq!(path, Some(Path::new("/fake/key")));
                assert!(pass.is_none());
                Ok(prepared())
            },
            |_| Ok(Arc::new(Session::default())),
        )
        .unwrap();
        assert!(matches!(result, ConnectionOutcome::Connected { .. }));
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn encrypted_key_reads_new_then_legacy_only_for_privatekey_metadata() {
        let store = Arc::new(MemoryStore::default());
        store
            .set(&ssh_account("test").unwrap(), " correct ")
            .unwrap();
        store.calls.lock().unwrap().clear();
        let result = connect_with(
            &entry(ServerAuthKind::PrivateKey),
            None,
            false,
            store.clone(),
            active(),
            |path, pass| encrypted(path.unwrap(), pass),
            |_| Ok(Arc::new(Session::default())),
        )
        .unwrap();
        assert!(matches!(result, ConnectionOutcome::Connected { .. }));
        assert_eq!(
            *store.calls.lock().unwrap(),
            vec![
                format!("get:{}", ssh_passphrase_account("test").unwrap()),
                "get:ssh/test".into()
            ]
        );
        store.calls.lock().unwrap().clear();
        let result = connect_with(
            &entry(ServerAuthKind::Password),
            Some((CredentialKind::Login, String::new())),
            false,
            store.clone(),
            active(),
            |_, pass| encrypted(Path::new("/selected/default"), pass),
            |_| panic!("无口令不连接"),
        )
        .unwrap();
        let ConnectionOutcome::Prompt { kind, .. } = result else {
            panic!("提示口令")
        };
        assert_eq!(
            kind,
            CredentialKind::PrivateKey {
                key_path: "/selected/default".into(),
                context: None
            }
        );
        assert_eq!(
            *store.calls.lock().unwrap(),
            vec![format!("get:{}", ssh_passphrase_account("test").unwrap())]
        );
    }

    #[test]
    fn supplied_passphrase_is_exact_path_and_saved_only_after_success() {
        let store = Arc::new(MemoryStore::default());
        let supplied = Some((
            CredentialKind::PrivateKey {
                key_path: "/prompt/key".into(),
                context: None,
            },
            " correct ".into(),
        ));
        let result = connect_with(
            &entry(ServerAuthKind::Password),
            supplied.clone(),
            true,
            store.clone(),
            active(),
            |path, pass| {
                assert_eq!(path, Some(Path::new("/prompt/key")));
                encrypted(path.unwrap(), pass)
            },
            |_| Err(anyhow::anyhow!("server rejected")),
        );
        assert!(result.is_err());
        assert!(store.calls.lock().unwrap().is_empty());
        connect_with(
            &entry(ServerAuthKind::Password),
            supplied,
            true,
            store.clone(),
            active(),
            |path, pass| encrypted(path.unwrap(), pass),
            |_| Ok(Arc::new(Session::default())),
        )
        .unwrap();
        assert_eq!(
            store
                .get(&ssh_passphrase_account("test").unwrap())
                .unwrap()
                .as_deref(),
            Some(" correct ")
        );
        assert_eq!(store.get(&ssh_account("test").unwrap()).unwrap(), None);
    }

    #[test]
    fn bad_passphrase_prompts_and_login_keeps_whitespace() {
        let store = Arc::new(MemoryStore::default());
        let result = connect_with(
            &entry(ServerAuthKind::PrivateKey),
            Some((
                CredentialKind::PrivateKey {
                    key_path: "/prompt/key".into(),
                    context: None,
                },
                "wrong".into(),
            )),
            false,
            store.clone(),
            active(),
            |path, pass| encrypted(path.unwrap(), pass),
            |_| panic!("不连接"),
        )
        .unwrap();
        assert!(matches!(
            result,
            ConnectionOutcome::Prompt {
                kind: CredentialKind::PrivateKey { .. },
                warning: Some(_)
            }
        ));
        for auth in [
            ServerAuthKind::Password,
            ServerAuthKind::KeyboardInteractive,
        ] {
            connect_with(
                &entry(auth),
                Some((CredentialKind::Login, "  ".into())),
                false,
                store.clone(),
                active(),
                |_, _| panic!("空白不是空串"),
                |auth| {
                    match auth {
                        AuthMethod::Password { password } => assert_eq!(password, "  "),
                        AuthMethod::KeyboardInteractive { secret } => assert_eq!(secret, "  "),
                        _ => panic!("应密码认证"),
                    }
                    Ok(Arc::new(Session::default()))
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn cancelled_connection_closes_session_and_never_saves() {
        let store = Arc::new(MemoryStore::default());
        let session = Arc::new(Session::default());
        let flag = active();
        let result = connect_with(
            &entry(ServerAuthKind::Password),
            Some((CredentialKind::Login, "secret".into())),
            true,
            store.clone(),
            flag.clone(),
            |_, _| unreachable!(),
            |_| {
                flag.store(false, Ordering::SeqCst);
                Ok(session.clone())
            },
        );
        assert!(result.is_err());
        assert!(session.0.load(Ordering::SeqCst));
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn saving_failure_preserves_connection_with_safe_warning() {
        let store = Arc::new(MemoryStore::default());
        store.fail.store(true, Ordering::SeqCst);
        let result = connect_with(
            &entry(ServerAuthKind::Password),
            Some((CredentialKind::Login, "secret".into())),
            true,
            store,
            active(),
            |_, _| unreachable!(),
            |_| Ok(Arc::new(Session::default())),
        )
        .unwrap();
        let ConnectionOutcome::Connected { warning, .. } = result else {
            panic!("保留连接")
        };
        assert!(warning.is_some());
        assert!(!warning.unwrap().contains("sensitive-store-error"));
    }

    #[test]
    fn rename_copies_slots_without_deletion_and_new_secret_only_changes_own_slot() {
        let store = Arc::new(MemoryStore::default());
        let original = entry(ServerAuthKind::PrivateKey);
        store
            .set(&ssh_account("test").unwrap(), "legacy-passphrase")
            .unwrap();
        store
            .set(&ssh_passphrase_account("test").unwrap(), "new-passphrase")
            .unwrap();
        let mut meta = original.clone();
        meta.name = "renamed".into();
        meta.auth = ServerAuthKind::Password;
        update_server_secrets(
            store.clone(),
            Some(&original),
            &meta,
            " new login ",
            active(),
        )
        .unwrap();
        assert_eq!(
            store
                .get(&ssh_account("renamed").unwrap())
                .unwrap()
                .as_deref(),
            Some(" new login ")
        );
        assert_eq!(
            store
                .get(&ssh_passphrase_account("renamed").unwrap())
                .unwrap()
                .as_deref(),
            Some("new-passphrase")
        );
        assert_eq!(
            store.get(&ssh_account("test").unwrap()).unwrap().as_deref(),
            Some("legacy-passphrase")
        );
        cleanup_renamed_server_secrets(store.as_ref(), &original, &meta).unwrap();
        assert!(store.get(&ssh_account("test").unwrap()).unwrap().is_none());
        assert!(
            store
                .get(&ssh_passphrase_account("test").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_unlock_continues_remaining_candidate_without_rescanning_first() {
        let store = Arc::new(MemoryStore::default());
        let mut meta = entry(ServerAuthKind::PrivateKey);
        meta.key_path = None;
        let outcome = connect_with(
            &meta,
            None,
            false,
            store.clone(),
            active(),
            |path, pass| encrypted(path.unwrap(), pass),
            |_| panic!("等待口令"),
        )
        .unwrap();
        let ConnectionOutcome::Prompt { kind, .. } = outcome else {
            panic!("需要口令")
        };
        let calls = Mutex::new(Vec::new());
        connect_with(
            &meta,
            Some((kind, " correct ".into())),
            false,
            store,
            active(),
            |path, pass| {
                let path = path.unwrap();
                calls.lock().unwrap().push((
                    path.file_name().unwrap().to_string_lossy().to_string(),
                    pass.is_some(),
                ));
                Ok(prepared())
            },
            |_| Ok(Arc::new(Session::default())),
        )
        .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("id_ed25519".into(), true), ("id_rsa".into(), false)]
        );
    }

    #[test]
    fn default_unlock_keeps_previously_loaded_key_in_fixed_order() {
        let store = Arc::new(MemoryStore::default());
        let mut meta = entry(ServerAuthKind::PrivateKey);
        meta.key_path = None;
        let first = connect_with(
            &meta,
            None,
            false,
            store.clone(),
            active(),
            |path, pass| {
                let path = path.expect("默认候选必须逐文件准备");
                if path.ends_with("id_ed25519") {
                    Ok(prepared())
                } else {
                    encrypted(path, pass)
                }
            },
            |_| panic!("等待解锁"),
        )
        .unwrap();
        let ConnectionOutcome::Prompt { kind, .. } = first else {
            panic!("需要口令")
        };
        let CredentialKind::PrivateKey {
            context: Some(ref context),
            ..
        } = kind
        else {
            panic!("必须保留默认上下文")
        };
        assert_eq!(context.prepared.len(), 1, "保留已加载的第一把钥匙");
        connect_with(
            &meta,
            Some((kind, " correct ".into())),
            false,
            store,
            active(),
            |path, pass| encrypted(path.unwrap(), pass),
            |auth| {
                let AuthMethod::PreparedPrivateKey { keys } = auth else {
                    panic!("预加载")
                };
                assert!(keys.is_empty(), "注入的占位密钥集合为空");
                Ok(Arc::new(Session::default()))
            },
        )
        .unwrap();
    }

    #[test]
    fn cancellation_between_migration_and_delete_preserves_old_slot() {
        let store = Arc::new(MemoryStore::default());
        store.set(&ssh_account("test").unwrap(), "legacy").unwrap();
        let flag = active();
        *store.cancel_after_set.lock().unwrap() = Some(flag.clone());
        let original = entry(ServerAuthKind::PrivateKey);
        let mut meta = original.clone();
        meta.auth = ServerAuthKind::Password;
        assert!(
            update_server_secrets(store.clone(), Some(&original), &meta, "new password", flag)
                .is_err()
        );
        assert_eq!(
            store.get(&ssh_account("test").unwrap()).unwrap().as_deref(),
            Some("legacy")
        );
        assert!(
            !store
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("delete:"))
        );
    }

    #[test]
    fn switching_keeps_existing_passphrase_and_accepts_only_new_login() {
        let store = Arc::new(MemoryStore::default());
        store.set(&ssh_account("test").unwrap(), "legacy").unwrap();
        store
            .set(&ssh_passphrase_account("test").unwrap(), "independent")
            .unwrap();
        let original = entry(ServerAuthKind::PrivateKey);
        let mut meta = original.clone();
        meta.auth = ServerAuthKind::Password;
        update_server_secrets(
            store.clone(),
            Some(&original),
            &meta,
            " genuine login ",
            active(),
        )
        .unwrap();
        assert_eq!(
            store.get(&ssh_account("test").unwrap()).unwrap().as_deref(),
            Some(" genuine login ")
        );
        assert_eq!(
            store
                .get(&ssh_passphrase_account("test").unwrap())
                .unwrap()
                .as_deref(),
            Some("independent")
        );
    }

    #[test]
    fn saving_cancelled_before_start_never_mutates_store() {
        let store = Arc::new(MemoryStore::default());
        let flag = Arc::new(AtomicBool::new(false));
        let meta = entry(ServerAuthKind::Password);
        assert!(update_server_secrets(store.clone(), None, &meta, "new", flag).is_err());
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn switching_legacy_privatekey_never_reuses_passphrase_as_login() {
        for renamed in [false, true] {
            for auth in [
                ServerAuthKind::Password,
                ServerAuthKind::KeyboardInteractive,
            ] {
                let store = Arc::new(MemoryStore::default());
                store
                    .set(&ssh_account("test").unwrap(), "legacy-private-secret")
                    .unwrap();
                let original = entry(ServerAuthKind::PrivateKey);
                let mut meta = original.clone();
                meta.auth = auth;
                if renamed {
                    meta.name = "renamed".into();
                }
                update_server_secrets(store.clone(), Some(&original), &meta, "", active()).unwrap();
                assert!(
                    store
                        .get(&ssh_account(&meta.name).unwrap())
                        .unwrap()
                        .is_none(),
                    "不能把私钥解密口令当成登录密码"
                );
                assert_eq!(
                    store
                        .get(&ssh_passphrase_account(&meta.name).unwrap())
                        .unwrap()
                        .as_deref(),
                    Some("legacy-private-secret")
                );
            }
        }
    }

    #[test]
    fn rename_privatekey_never_copies_legacy_into_login() {
        let store = Arc::new(MemoryStore::default());
        store.set(&ssh_account("test").unwrap(), "legacy").unwrap();
        let original = entry(ServerAuthKind::PrivateKey);
        let mut meta = original.clone();
        meta.name = "renamed".into();
        update_server_secrets(
            store.clone(),
            Some(&original),
            &meta,
            "new-passphrase",
            active(),
        )
        .unwrap();
        assert!(
            store
                .get(&ssh_account("renamed").unwrap())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .get(&ssh_passphrase_account("renamed").unwrap())
                .unwrap()
                .as_deref(),
            Some("new-passphrase")
        );
    }

    #[test]
    fn no_secret_no_rename_is_noop_and_legacy_privatekey_is_migrated() {
        let store = Arc::new(MemoryStore::default());
        let original = entry(ServerAuthKind::PrivateKey);
        update_server_secrets(store.clone(), Some(&original), &original, "", active()).unwrap();
        assert!(store.calls.lock().unwrap().is_empty());
        store.set(&ssh_account("test").unwrap(), "legacy").unwrap();
        let mut meta = original.clone();
        meta.name = "renamed".into();
        update_server_secrets(store.clone(), Some(&original), &meta, "", active()).unwrap();
        assert_eq!(
            store
                .get(&ssh_passphrase_account("renamed").unwrap())
                .unwrap()
                .as_deref(),
            Some("legacy")
        );
        delete_server_secrets(store.as_ref(), "renamed").unwrap();
        assert!(
            store
                .get(&ssh_account("renamed").unwrap())
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get(&ssh_passphrase_account("renamed").unwrap())
                .unwrap()
                .is_none()
        );
    }
}
