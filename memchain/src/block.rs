pub struct Block<'bc> {
    state: State<'bc>,
    pending_transaction: Option<PendingTransaction<'bc>>,
    completed_transactions: Vec<Receipt>,
}

struct PendingTransaction<'bc> {
    state: State<'bc>,
    logs: Vec<Log>,
    call_stack: Vec<Transaction>,
    ret_buf: Vec<u8>,
    err_buf: Vec<u8>,
    outcome: TransactionOutcome,
}

impl<'bc> PendingTransaction<'bc> {
    fn input(&self) -> Vec<u8> {
        self.call_stack.last().unwrap().input.to_vec()
    }
}

impl<'bc> Block<'bc> {
    pub fn logs(&self) -> Vec<&Log> {
        self.completed_transactions
            .iter()
            .flat_map(|tx| tx.logs.iter())
            .collect()
    }

    fn pending_transaction(&self) -> Option<&PendingTransaction> {
        self.pending_transaction.as_ref()
    }

    fn pending_transaction_mut(&mut self) -> Option<&mut PendingTransaction<'bc>> {
        self.pending_transaction.as_mut()
    }

    fn current_state(&self) -> &State<'bc> {
        match &self.pending_transaction {
            Some(ptx) => &ptx.state,
            None => &self.state,
        }
    }
}

impl<'bc> KVStore for Block<'bc> {
    fn contains(&self, addr: &Address, key: &[u8]) -> bool {
        self.current_state()
            .get(addr)
            .map(|acct| acct.storage.contains_key(key))
            .unwrap_or(false)
    }

    fn size(&self, address: &Address, key: &[u8]) -> u64 {
        self.get(address, key).map(|v| v.len() as u64).unwrap_or(0)
    }

    fn get(&self, addr: &Address, key: &[u8]) -> Option<&[u8]> {
        self.current_state()
            .get(addr)
            .and_then(|acct| acct.storage.get(key))
            .map(Vec::as_slice)
    }

    fn set(&mut self, addr: &Address, key: Vec<u8>, value: Vec<u8>) {
        self.pending_transaction_mut()
            .and_then(|tx| {
                let callee = &tx.call_stack.last().unwrap().callee;
                let mut addr = addr;
                if addr == &Address::default() {
                    addr = callee;
                }
                if addr == callee {
                    tx.state.get_mut(&addr)
                } else {
                    // capabilities to other services' storage are unimplemented
                    // would panic if there were a way to catch it?
                    if !tx.outcome.reverted() {
                        tx.outcome = TransactionOutcome::InvalidOperation;
                    }
                    None
                }
            })
            .map(|acct| acct.to_mut().storage.insert(key, value));
    }
}

impl<'bc> BlockchainIntrinsics for Block<'bc> {
    fn transact(
        &mut self,
        mut caller: Address,
        callee: Address,
        value: U256,
        input: Vec<u8>,
        gas: U256,
        gas_price: U256,
    ) {
        let mut receipt = Receipt {
            caller,
            callee,
            value,
            gas_used: gas,
            ret_buf: Vec::new(),
            err_buf: Vec::new(),
            logs: Vec::new(),
            outcome: TransactionOutcome::Success,
        };

        macro_rules! early_return {
            ($outcome:ident) => {{
                match &mut self.pending_transaction {
                    Some(ptx) => {
                        ptx.outcome = TransactionOutcome::$outcome;
                    }
                    None => {
                        receipt.outcome = TransactionOutcome::$outcome;
                        receipt.ret_buf.clear();
                        receipt.logs.clear();
                        self.completed_transactions.push(receipt);
                    }
                }
                return;
            }};
        }

        if let Some(ptx) = &self.pending_transaction {
            let prev_callee = ptx.call_stack.last().unwrap().callee;
            if caller == Address::default() {
                caller = prev_callee;
            } else if caller != prev_callee {
                early_return!(InvalidCaller);
            }
        };

        // Check callee existence here so that caller balances can be modified
        // and dropped before `&mut callee` is required.
        if !self.state.contains_key(&callee) {
            early_return!(NoCallee);
        }

        let caller_acct = match self.state.get_mut(&caller) {
            Some(acct) => acct.to_mut(),
            None => early_return!(NoCaller),
        };

        if gas < U256::from(BASE_GAS) {
            early_return!(InsufficientGas);
        }

        if caller_acct.balance < (gas * gas_price + value) {
            caller_acct.balance = U256::zero();
            early_return!(InsuffientFunds)
        }
        caller_acct.balance -= gas * gas_price;

        let callee_acct = match self.state.get_mut(&callee) {
            Some(acct) => acct.to_mut(),
            None => early_return!(NoCallee),
        };

        let tx = Transaction {
            caller,
            callee,
            value,
            input,
            gas,
        };

        let main_fn = callee_acct.main;

        match &mut self.pending_transaction {
            Some(ptx) => {
                ptx.call_stack.push(tx);
            }
            None => {
                self.pending_transaction = Some(PendingTransaction {
                    state: self.state.clone(),
                    logs: Vec::new(),
                    call_stack: vec![tx],
                    ret_buf: Vec::new(),
                    err_buf: Vec::new(),
                    outcome: TransactionOutcome::Success,
                })
            }
        }

        if let Some(main) = main_fn {
            let bci: &mut dyn BlockchainIntrinsics = self;
            let errno = main(unsafe { std::mem::transmute::<_, &'static mut _>(bci) });
            if errno == 0 {
                // success
                let mut ptx = self.pending_transaction_mut().unwrap();
                ptx.state.get_mut(&caller).unwrap().to_mut().balance -= value;
                ptx.state.get_mut(&callee).unwrap().to_mut().balance += value;
            } else {
                self.pending_transaction.as_mut().unwrap().outcome = TransactionOutcome::Aborted;
            }
        }

        self.pending_transaction.as_mut().unwrap().call_stack.pop();
        if self
            .pending_transaction
            .as_ref()
            .unwrap()
            .call_stack
            .is_empty()
        {
            let ptx = self.pending_transaction.take().unwrap();
            receipt.outcome = ptx.outcome;
            receipt.ret_buf = ptx.ret_buf;
            receipt.err_buf = ptx.err_buf;
            receipt.logs = ptx.logs;
            if receipt.outcome.reverted() {
                receipt.ret_buf.clear();
                receipt.logs.clear();
            } else {
                self.state = ptx.state;
            }
            self.completed_transactions.push(receipt);
        }
    }

    fn fetch_input(&self) -> Vec<u8> {
        self.pending_transaction()
            .map(|tx| tx.input().clone())
            .unwrap_or_default()
    }

    fn input_len(&self) -> u64 {
        self.pending_transaction()
            .map(|tx| tx.input().len() as u64)
            .unwrap_or_default()
    }

    fn ret(&mut self, data: Vec<u8>) {
        if let Some(tx) = self.pending_transaction_mut() {
            tx.ret_buf = data
        }
    }

    fn err(&mut self, data: Vec<u8>) {
        if let Some(tx) = self.pending_transaction_mut() {
            tx.err_buf = data
        }
    }

    fn fetch_ret(&self) -> Vec<u8> {
        match &self.pending_transaction {
            Some(ptx) => ptx.ret_buf.to_vec(),
            None => self
                .completed_transactions
                .last()
                .map(|tx| tx.ret_buf.clone())
                .unwrap_or_default(),
        }
    }

    fn ret_len(&self) -> u64 {
        self.fetch_ret().len() as u64
    }

    fn fetch_err(&self) -> Vec<u8> {
        match &self.pending_transaction {
            Some(ptx) => ptx.err_buf.to_vec(),
            None => self
                .completed_transactions
                .last()
                .map(|tx| tx.err_buf.clone())
                .unwrap_or_default(),
        }
    }

    fn err_len(&self) -> u64 {
        self.fetch_err().len() as u64
    }

    fn emit(&mut self, topics: Vec<[u8; 32]>, data: Vec<u8>) {
        if let Some(tx) = self.pending_transaction_mut() {
            tx.logs.push(Log { topics, data })
        }
    }

    fn code_at(&self, addr: &Address) -> Option<&[u8]> {
        self.current_state()
            .get(&addr)
            .map(|acct| acct.code.as_slice())
    }

    fn code_len(&self, addr: &Address) -> u64 {
        self.current_state()
            .get(&addr)
            .map(|acct| acct.code.len() as u64)
            .unwrap_or_default()
    }

    fn metadata_at(&self, addr: &Address) -> Option<AccountMetadata> {
        self.current_state().get(&addr).map(|acct| AccountMetadata {
            balance: acct.balance,
            expiry: acct.expiry,
        })
    }

    fn value(&self) -> U256 {
        self.pending_transaction()
            .map(|tx| tx.call_stack.last().unwrap().value)
            .expect("No pending transaction.")
    }

    fn gas(&self) -> U256 {
        self.pending_transaction()
            .map(|tx| tx.call_stack.last().unwrap().gas)
            .expect("No pending transaction.")
    }

    fn sender(&self) -> Address {
        self.pending_transaction()
            .map(|tx| tx.call_stack.last().unwrap().caller)
            .expect("No pending transaction.")
    }
}