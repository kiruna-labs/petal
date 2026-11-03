//! Stream keys in the OS credential store: Windows Credential Manager and the
//! Apple Keychain, under the service `com.petal.app.stream`, one item per
//! saved destination.
//!
//! Direct platform calls rather than the `keyring` crate: its v4 split pulls
//! a separate store crate per platform, while the two calls needed here are a
//! few lines each over crates already in the build (`windows`,
//! `security-framework`). [`SecretStore`] is the seam the tests fake.

pub const STREAM_KEY_SERVICE: &str = "com.petal.app.stream";

pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    /// Deleting a missing item is not an error.
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The platform credential store.
pub fn platform_store() -> Box<dyn SecretStore> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows_store::CredentialManager)
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos_store::Keychain)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Box::new(Unsupported)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
struct Unsupported;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl SecretStore for Unsupported {
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Err("No credential store on this platform".into())
    }
    fn get(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows_store {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    use super::{SecretStore, STREAM_KEY_SERVICE};

    pub struct CredentialManager;

    fn target(account: &str) -> HSTRING {
        HSTRING::from(format!("{STREAM_KEY_SERVICE}/{account}"))
    }

    fn not_found(error: &windows::core::Error) -> bool {
        error.code() == ERROR_NOT_FOUND.to_hresult()
    }

    impl SecretStore for CredentialManager {
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            let mut target_wide: Vec<u16> = format!("{STREAM_KEY_SERVICE}/{account}")
                .encode_utf16()
                .chain([0])
                .collect();
            let mut blob = secret.as_bytes().to_vec();
            let credential = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target_wide.as_mut_ptr()),
                CredentialBlobSize: u32::try_from(blob.len()).map_err(|_| "Stream key too long")?,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            // SAFETY: every pointer in `credential` borrows a live local
            // buffer for the duration of the call; CredWriteW copies them.
            unsafe { CredWriteW(&credential, 0) }
                .map_err(|error| format!("Couldn't save the stream key: {}", error.message()))
        }

        fn get(&self, account: &str) -> Result<Option<String>, String> {
            let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: CredReadW writes an allocation we free with CredFree.
            match unsafe { CredReadW(&target(account), CRED_TYPE_GENERIC, None, &mut credential) } {
                Ok(()) => {}
                Err(error) if not_found(&error) => return Ok(None),
                Err(error) => {
                    return Err(format!("Couldn't read the stream key: {}", error.message()))
                }
            }
            // SAFETY: on success `credential` is a valid CREDENTIALW whose
            // blob is `CredentialBlobSize` bytes.
            let secret = unsafe {
                let record = &*credential;
                let bytes = if record.CredentialBlob.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(
                        record.CredentialBlob,
                        record.CredentialBlobSize as usize,
                    )
                };
                let secret = String::from_utf8(bytes.to_vec());
                CredFree(credential.cast());
                secret
            };
            secret
                .map(Some)
                .map_err(|_| "The saved stream key is unreadable".into())
        }

        fn delete(&self, account: &str) -> Result<(), String> {
            // SAFETY: plain call with an owned wide string.
            match unsafe { CredDeleteW(&target(account), CRED_TYPE_GENERIC, None) } {
                Ok(()) => Ok(()),
                Err(error) if not_found(&error) => Ok(()),
                Err(error) => Err(format!(
                    "Couldn't delete the stream key: {}",
                    error.message()
                )),
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_store {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::{SecretStore, STREAM_KEY_SERVICE};

    /// errSecItemNotFound.
    const ITEM_NOT_FOUND: i32 = -25300;

    pub struct Keychain;

    impl SecretStore for Keychain {
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            set_generic_password(STREAM_KEY_SERVICE, account, secret.as_bytes())
                .map_err(|error| format!("Couldn't save the stream key: {error}"))
        }

        fn get(&self, account: &str) -> Result<Option<String>, String> {
            match get_generic_password(STREAM_KEY_SERVICE, account) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| "The saved stream key is unreadable".into()),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
                Err(error) => Err(format!("Couldn't read the stream key: {error}")),
            }
        }

        fn delete(&self, account: &str) -> Result<(), String> {
            match delete_generic_password(STREAM_KEY_SERVICE, account) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
                Err(error) => Err(format!("Couldn't delete the stream key: {error}")),
            }
        }
    }
}

/// In-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct FakeStore {
    pub items: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Make `delete` fail, as a locked keychain would.
    pub fail_delete: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SecretStore for FakeStore {
    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.items
            .lock()
            .unwrap()
            .insert(account.into(), secret.into());
        Ok(())
    }
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.items.lock().unwrap().get(account).cloned())
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        if self.fail_delete.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Couldn't delete the stream key: locked".into());
        }
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// Round-trips a throwaway item through the real Credential Manager.
    #[test]
    #[ignore = "writes to the real Credential Manager"]
    fn credential_manager_round_trip() {
        let store = platform_store();
        let account = format!("test-{}", std::process::id());
        store.set(&account, "live_abc?x=1").unwrap();
        assert_eq!(
            store.get(&account).unwrap().as_deref(),
            Some("live_abc?x=1")
        );
        store.set(&account, "rotated").unwrap();
        assert_eq!(store.get(&account).unwrap().as_deref(), Some("rotated"));
        store.delete(&account).unwrap();
        assert_eq!(store.get(&account).unwrap(), None);
        store.delete(&account).unwrap();
    }
}
