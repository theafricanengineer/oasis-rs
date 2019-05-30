use std::{
    borrow::Cow,
    ffi::{c_void, CStr},
    slice,
};

use blockchain_traits::Blockchain;
use mantle_types::Address;

use crate::{Account, Memchain};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CAccount {
    address: Address,
    balance: u64,
    code: CSlice<u8>,
    /// Seconds since unix epoch. A value of 0 represents no expiry.
    expiry: u64,
    /// Pointer to fat pointer to callable main function.
    /// Set to nullptr if account has no code.
    main: extern "C" fn(*const *mut dyn Blockchain<Address = Address>) -> u16,
    storage: CSlice<CStorageItem>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CStorageItem {
    key: CSlice<u8>,
    value: CSlice<u8>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CSlice<T> {
    pub base: *const T,
    pub len: u64,
}

impl<T> CSlice<T> {
    unsafe fn as_slice(&self) -> &'static [T] {
        slice::from_raw_parts(self.base, self.len as usize)
    }
}

impl<T, S: AsRef<[T]>> From<S> for CSlice<T> {
    fn from(sl: S) -> Self {
        let sl = sl.as_ref();
        Self {
            base: sl.as_ptr(),
            len: sl.len() as u64,
        }
    }
}

impl From<CAccount> for Account {
    fn from(ca: CAccount) -> Self {
        Self {
            balance: ca.balance,
            code: unsafe { ca.code.as_slice() }.to_vec(),
            storage: unsafe { ca.storage.as_slice() }
                .iter()
                .map(|itm| unsafe { (itm.key.as_slice().to_vec(), itm.value.as_slice().to_vec()) })
                .collect(),
            expiry: if ca.expiry == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(ca.expiry))
            },
            main: if (ca.main as *const c_void).is_null() {
                None
            } else {
                Some(ca.main)
            },
        }
    }
}

#[repr(u8)]
#[derive(Debug, PartialEq, Eq)]
pub enum ErrNo {
    Success,
    NoAccount,
    AccountExists,
    NoKey,
    NoTx,
}

#[no_mangle]
pub unsafe extern "C" fn memchain_create(
    name: *const CStr,
    genesis_accounts: CSlice<CAccount>,
) -> *mut Memchain<'static> {
    let genesis_state = genesis_accounts
        .as_slice()
        .iter()
        .map(|ca| (ca.address, Cow::Owned(Account::from(*ca))))
        .collect();
    Box::into_raw(Box::new(Memchain::new(
        (*name).to_str().unwrap().to_string(),
        genesis_state,
    )))
}

#[no_mangle]
pub unsafe extern "C" fn memchain_destroy(memchain: *mut Memchain) {
    std::mem::drop(Box::from_raw(memchain))
}

/// Adds a new account to the blockchain at the current block.
#[no_mangle]
pub unsafe extern "C" fn memchain_create_account(
    memchain: *mut Memchain,
    new_account: *const CAccount,
) -> ErrNo {
    let bc = &mut *memchain;
    if bc
        .last_block_mut()
        .create_account((*new_account).address, Account::from(*new_account))
    {
        ErrNo::Success
    } else {
        ErrNo::AccountExists
    }
}

/// Retrieves a value from storage at the current block through the current transaction.
#[no_mangle]
pub unsafe extern "C" fn memchain_storage_at(
    memchain: *mut Memchain,
    address: Address,
    key: CSlice<u8>,
    value: *mut CSlice<u8>,
) -> ErrNo {
    let bc = &*memchain;
    let account = match bc.current_state().get(&address) {
        Some(account) => account,
        None => return ErrNo::NoAccount,
    };
    match account.storage.get(key.as_slice()) {
        Some(val) => {
            *value = val.as_slice().into();
            ErrNo::Success
        }
        None => ErrNo::NoKey,
    }
}

/// Creates a new block.
#[no_mangle]
pub unsafe extern "C" fn memchain_create_block(memchain: *mut Memchain) -> ErrNo {
    (&mut *memchain).create_block();
    ErrNo::Success
}

/// Executes a transaction.
#[no_mangle]
pub unsafe extern "C" fn memchain_transact(
    memchain: *mut Memchain,
    caller: *const Address,
    callee: *const Address,
    value: u64,
    input: CSlice<u8>,
    gas: u64,
    gas_price: u64,
) -> ErrNo {
    let bc = &mut *memchain;
    bc.last_block_mut().transact(
        *caller,
        *callee,
        value,
        input.as_slice().to_vec(),
        gas,
        gas_price,
    );
    ErrNo::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::CString;

    extern "C" fn nop_main(_: *const *mut dyn Blockchain<Address = Address>) -> u16 {
        0
    }

    #[test]
    fn account_storage() {
        let key = "hello";
        let v_0 = "world";
        let v_1 = "general kenobi";

        unsafe {
            let account_1_storage = vec![CStorageItem {
                key: key.into(),
                value: v_0.into(),
            }];
            let genesis_accounts = vec![CAccount {
                address: Address::from(1),
                balance: 1,
                code: vec![].as_slice().into(),
                expiry: 0,
                main: nop_main,
                storage: account_1_storage.as_slice().into(),
            }];

            let handle = memchain_create(
                CString::new("memchain").unwrap().as_c_str(),
                genesis_accounts.as_slice().into(),
            );

            let account_2_storage = vec![CStorageItem {
                key: key.into(),
                value: v_1.into(),
            }];
            let account_2 = CAccount {
                address: Address::from(2),
                balance: 2,
                code: "\0asm this is not wasm".as_bytes().into(),
                expiry: 0,
                main: nop_main,
                storage: account_2_storage.as_slice().into(),
            };

            let create_account_2 = || memchain_create_account(handle, &account_2 as *const _);
            assert_eq!(create_account_2(), ErrNo::Success);
            assert_eq!(create_account_2(), ErrNo::AccountExists);

            let mut value_buf = std::mem::MaybeUninit::uninit();
            macro_rules! storage_at {
                ($addr:expr, $key:expr) => {
                    memchain_storage_at(handle, $addr, $key.into(), value_buf.as_mut_ptr())
                };
            }

            assert_eq!(storage_at!(Address::from(1), key), ErrNo::Success);
            assert_eq!(value_buf.assume_init().as_slice(), v_0.as_bytes());

            assert_eq!(storage_at!(Address::from(2), key), ErrNo::Success);
            assert_eq!(value_buf.assume_init().as_slice(), v_1.as_bytes());

            assert_eq!(storage_at!(Address::from(0), key), ErrNo::NoAccount);
            assert_eq!(storage_at!(Address::from(1), b"yodawg"), ErrNo::NoKey);

            memchain_destroy(handle);
        }
    }
}
