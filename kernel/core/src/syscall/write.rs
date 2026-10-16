// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::{
    fs,
    fs::file::file_table::{RawFileDesc, get_file_fast},
    prelude::*,
};

pub(super) fn sys_write(
    raw_fd: RawFileDesc,
    user_buf_ptr: Vaddr,
    user_buf_len: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    debug!(
        "raw_fd = {}, user_buf_ptr = 0x{:x}, user_buf_len = 0x{:x}",
        raw_fd, user_buf_ptr, user_buf_len
    );

    // K1 MUSE Book --- (k1.diag=stderr: echo fd 2 before it disappears into journald)
    // `k1.diag=stderr`: echo what goes to fd 2 before it disappears into
    // journald's socket. A separate reader over (a prefix of) the same user
    // buffer; the syscall's own reader below is untouched.
    if raw_fd == 2 && user_buf_len != 0 && super::proc_diag::stderr_on() {
        let len = user_buf_len.min(super::proc_diag::STDERR_TEE_MAX);
        if let Ok(reader) = ctx.user_space().reader(user_buf_ptr, len) {
            super::proc_diag::tee_stderr(ctx, &mut [reader]);
        }
    }
    // K1 MUSE Book ---

    let mut file_table = ctx.thread_local.borrow_file_table_mut();
    let file = get_file_fast!(&mut file_table, raw_fd.try_into()?);

    // According to <https://man7.org/linux/man-pages/man2/write.2.html>, if
    // the user specified an empty buffer, we should detect errors by checking
    // the file descriptor. If no errors detected, return 0 successfully.
    let write_len = {
        if user_buf_len != 0 {
            let user_space = ctx.user_space();
            let mut reader = user_space.reader(user_buf_ptr, user_buf_len)?;
            file.write(&mut reader)
        } else {
            file.write_bytes(&[])
        }
    }
    .map_err(|err| match err.error() {
        Errno::EINTR => Error::new(Errno::ERESTARTSYS),
        _ => err,
    })?;

    if write_len > 0 {
        fs::vfs::notify::on_modify(&file);
    }
    Ok(SyscallReturn::Return(write_len as _))
}
