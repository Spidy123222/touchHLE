/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! POSIX I/O functions (`fcntl.h`, parts of `unistd.h`, etc)

use crate::abi::VAList;
use crate::dyld::{export_c_func, FunctionExports};
use crate::fs::{GuestFile, GuestOpenOptions, GuestPath};
use crate::mem::{ConstPtr, GuestISize, GuestUSize, MutVoidPtr};
use crate::Environment;
use std::io::Read;

#[derive(Default)]
pub struct State {
    /// File descriptors _other than stdin, stdout, and stderr_
    files: Vec<Option<PosixFileHostObject>>,
}
impl State {
    fn file_for_fd(&mut self, fd: FileDescriptor) -> Option<&mut PosixFileHostObject> {
        self.files
            .get_mut(fd_to_file_idx(fd))
            .and_then(|file_or_none| file_or_none.as_mut())
    }
}

struct PosixFileHostObject {
    file: GuestFile,
}

// TODO: stdin/stdout/stderr handling somehow
fn file_idx_to_fd(idx: usize) -> FileDescriptor {
    FileDescriptor::try_from(idx)
        .unwrap()
        .checked_add(NORMAL_FILENO_BASE)
        .unwrap()
}
fn fd_to_file_idx(fd: FileDescriptor) -> usize {
    fd.checked_sub(NORMAL_FILENO_BASE).unwrap() as usize
}

/// File descriptor type. This alias is for readability, POSIX just uses `int`.
type FileDescriptor = i32;
#[allow(dead_code)]
const STDIN_FILENO: FileDescriptor = 0;
#[allow(dead_code)]
const STDOUT_FILENO: FileDescriptor = 1;
const STDERR_FILENO: FileDescriptor = 2;
const NORMAL_FILENO_BASE: FileDescriptor = STDERR_FILENO + 1;

/// Flags bitfield for `open`. This alias is for readability, POSIX just uses
/// `int`.
type OpenFlag = i32;
const O_RDONLY: OpenFlag = 0x0;
const O_WRONLY: OpenFlag = 0x1;
const O_RDWR: OpenFlag = 0x2;
const O_ACCMODE: OpenFlag = O_RDWR | O_WRONLY | O_RDONLY;

const O_NONBLOCK: OpenFlag = 0x4;
const O_APPEND: OpenFlag = 0x8;
const O_NOFOLLOW: OpenFlag = 0x100;
const O_CREAT: OpenFlag = 0x200;
const O_TRUNC: OpenFlag = 0x400;
const O_EXCL: OpenFlag = 0x800;

fn open(env: &mut Environment, path: ConstPtr<u8>, flags: i32, _args: VAList) -> FileDescriptor {
    assert!([O_RDONLY, O_WRONLY, O_RDWR].contains(&(flags & O_ACCMODE)));
    // TODO: support more flags, this list is not complete
    assert!(
        flags & !(O_ACCMODE | O_NONBLOCK | O_APPEND | O_NOFOLLOW | O_CREAT | O_TRUNC | O_EXCL) == 0
    );
    // TODO: symlinks don't exist in the FS yet, so we can't "not follow" them.
    // (Should we just ignore this?)
    assert!(flags & O_NOFOLLOW == 0);
    // TODO: exclusive mode not implemented yet
    assert!(flags & O_EXCL == 0);

    // TODO: respect the mode (in the variadic arguments) when creating a file
    // Note: NONBLOCK flag is ignored, assumption is all file I/O is fast
    let mut options = GuestOpenOptions::new();
    if (flags & (O_RDONLY | O_RDWR)) != 0 {
        options.read();
    }
    if (flags & (O_WRONLY | O_RDWR)) != 0 {
        options.write();
    }
    if (flags & O_APPEND) != 0 {
        options.append();
    }
    if (flags & O_CREAT) != 0 {
        options.create();
    }
    if (flags & O_TRUNC) != 0 {
        options.truncate();
    }

    match env.fs.open_with_options(
        GuestPath::new(&env.mem.cstr_at_utf8(path).unwrap()),
        options,
    ) {
        Ok(file) => {
            let host_object = PosixFileHostObject { file };

            let idx = if let Some(free_idx) = env
                .libc_state
                .posix_io
                .files
                .iter()
                .position(|f| f.is_none())
            {
                env.libc_state.posix_io.files[free_idx] = Some(host_object);
                free_idx
            } else {
                let idx = env.libc_state.posix_io.files.len();
                env.libc_state.posix_io.files.push(Some(host_object));
                idx
            };
            let fd = file_idx_to_fd(idx);
            log_dbg!("open({:?}, {:#x}) => {:?}", path, flags, fd);
            fd
        }
        Err(()) => {
            // TODO: set errno
            log!(
                "Warning: open({:?}, {:#x}) failed, returning -1",
                path,
                flags,
            );
            -1
        }
    }
}

fn read(
    env: &mut Environment,
    fd: FileDescriptor,
    buffer: MutVoidPtr,
    size: GuestUSize,
) -> GuestISize {
    // TODO: error handling for unknown fd?
    let file = env.libc_state.posix_io.file_for_fd(fd).unwrap();

    let buffer_slice = env.mem.bytes_at_mut(buffer.cast(), size);
    // TODO: handle errors
    match file.file.read(buffer_slice) {
        Ok(bytes_read) => {
            if bytes_read < buffer_slice.len() {
                log!(
                    "Warning: read({:?}, {:?}, {:#x}) read only {:#x} bytes",
                    fd,
                    buffer,
                    size,
                    bytes_read,
                );
            } else {
                log_dbg!(
                    "read({:?}, {:?}, {:#x}) => {:#x}",
                    fd,
                    buffer,
                    size,
                    bytes_read,
                );
            }
            bytes_read.try_into().unwrap()
        }
        Err(e) => {
            // TODO: set errno
            log!(
                "Warning: read({:?}, {:?}, {:#x}) encountered error {:?}, returning -1",
                fd,
                buffer,
                size,
                e,
            );
            -1
        }
    }
}

fn close(env: &mut Environment, fd: FileDescriptor) -> i32 {
    // TODO: error handling for unknown fd?
    let file = env.libc_state.posix_io.files[fd_to_file_idx(fd)]
        .take()
        .unwrap();
    // The actual closing of the file happens implicitly when `file` falls out
    // of scope. The return value is about whether flushing succeeds.
    match file.file.sync_all() {
        Ok(()) => {
            log_dbg!("close({:?}) => 0", fd);
            0
        }
        Err(_) => {
            // TODO: set errno
            log!("Warning: close({:?}) failed, returning -1", fd);
            -1
        }
    }
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(open(_, _, _)),
    export_c_func!(read(_, _, _)),
    export_c_func!(close(_)),
];
