#![no_std]
#![no_main]

extern crate alloc;

use user_lib::*;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("=== Permission System Test Program ===");

    // Test getting current user info
    println!("\n1. Get current user info:");
    let uid = getuid();
    let gid = getgid();
    let euid = geteuid();
    let egid = getegid();
    println!("UID: {}, GID: {}, EUID: {}, EGID: {}", uid, gid, euid, egid);

    // Test creating file
    println!("\n2. Create test file:");
    let test_file = "/test_permissions.txt";
    let fd = open(test_file, 0o644);
    if fd >= 0 {
        println!("File created successfully: {}", test_file);
        let content = b"This is a test file for permission testing.";
        let written = write(fd as usize, content);
        println!("Wrote {} bytes", written);
        close(fd as usize);
    } else {
        println!("Failed to create file: {} (error code: {})", test_file, fd);
    }

    // Test chmod
    println!("\n3. Test chmod (change file permissions):");
    let chmod_result = chmod(test_file, 0o755);
    if chmod_result == 0 {
        println!("chmod succeeded: set permission to 0755");
    } else {
        println!("chmod failed: error code {}", chmod_result);
    }

    // Test chown
    println!("\n4. Test chown (change file owner):");
    let chown_result = chown(test_file, 1000, 1000);
    if chown_result == 0 {
        println!("chown succeeded: set owner to UID=1000, GID=1000");
    } else {
        println!("chown failed: error code {}", chown_result);
    }

    // Test non-root user permission restrictions
    println!("\n5. Test user permission switching:");

    // First try to set to normal user
    let setuid_result = setuid(1000);
    if setuid_result == 0 {
        println!("Switched to UID=1000 successfully");

        // Show user info after switching
        let new_uid = getuid();
        let new_euid = geteuid();
        println!("After switch - UID: {}, EUID: {}", new_uid, new_euid);

        // Try to switch back to root (should fail)
        let back_to_root = setuid(0);
        if back_to_root == 0 {
            println!("❌ Warning: Normal user switched back to root successfully (this should not happen!)");
        } else {
            println!("✅ Correct: Normal user cannot switch back to root (error code: {})", back_to_root);
        }

        // Try to change permissions of another file (should fail)
        let chmod_fail = chmod("/etc/passwd", 0o777);
        if chmod_fail == 0 {
            println!("❌ Warning: Normal user changed system file permissions successfully (this should not happen!)");
        } else {
            println!("✅ Correct: Normal user cannot change system file permissions (error code: {})", chmod_fail);
        }

    } else {
        println!("setuid failed: error code {}", setuid_result);
    }

    // Test file permission check
    println!("\n6. Test file permission check:");

    // Create a read-only file
    let readonly_file = "/readonly_test.txt";
    let fd = open(readonly_file, 0o644);
    if fd >= 0 {
        write(fd as usize, b"readonly content");
        close(fd as usize);

        // Change to read-only permission
        chmod(readonly_file, 0o444);
        println!("Created read-only file: {}", readonly_file);

        // Try to open in write mode (should fail)
        let write_fd = open(readonly_file, 0o2); // O_WRONLY
        if write_fd >= 0 {
            println!("❌ Warning: Opened read-only file in write mode successfully (this should not happen!)");
            close(write_fd as usize);
        } else {
            println!("✅ Correct: Cannot open read-only file in write mode (error code: {})", write_fd);
        }

        // Try to open in read mode (should succeed)
        let read_fd = open(readonly_file, 0o0); // O_RDONLY
        if read_fd >= 0 {
            println!("✅ Correct: Opened read-only file in read mode successfully");
            close(read_fd as usize);
        } else {
            println!("❌ Error: Cannot open read-only file in read mode (error code: {})", read_fd);
        }
    }

    // Test directory permissions
    println!("\n7. Test directory permissions:");
    let test_dir = "/test_dir";
    let mkdir_result = mkdir(test_dir);
    if mkdir_result == 0 {
        println!("Created test directory: {}", test_dir);

        // Change directory permissions
        let chmod_dir = chmod(test_dir, 0o755);
        if chmod_dir == 0 {
            println!("Set directory permission to 0755 successfully");
        } else {
            println!("Failed to set directory permission: error code {}", chmod_dir);
        }

        // Change directory owner
        let chown_dir = chown(test_dir, 1000, 1000);
        if chown_dir == 0 {
            println!("Set directory owner successfully");
        } else {
            println!("Failed to set directory owner: error code {}", chown_dir);
        }
    }

    println!("\n=== Permission system test completed ===");
    0
}