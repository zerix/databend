// Copyright 2020-2021 The Datafuse Authors.
//
// SPDX-License-Identifier: Apache-2.0.

use common_exception::ErrorCode;
use common_exception::Result;

pub trait IMySQLEndpoint<Writer> {
    type Input;

    fn ok(data: Self::Input, writer: Writer) -> Result<()>;

    fn err(error: &ErrorCode, writer: Writer) -> Result<()>;

    fn on_action<F: Fn() -> Result<Self::Input>>(writer: Writer, fun: F) -> Result<()> {
        match fun() {
            Ok(data) => Self::ok(data, writer),
            Err(error) => {
                /// TODO: need refactor error code to enum
                let aborted_code = ErrorCode::AbortedSession("").code();
                match error.code() == aborted_code {
                    false => Self::err(&error, writer),
                    true => {
                        Self::err(&error, writer);
                        Err(error)
                    }
                }
            }
        }
    }
}
