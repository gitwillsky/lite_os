#![no_std]
#![no_main]

use user_lib::*;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("Execve functionality test program");
    println!("=================================");

    // Test 1: Basic execve with arguments
    println!("\n=== Test 1: Basic execve with arguments ===");
    
    let pid = fork();
    if pid == 0 {
        // Child process
        let args = ["args_test_program", "arg1", "arg2", "hello world"];
        let envs = ["PATH=/bin", "HOME=/root", "USER=testuser"];
        
        println!("Child: Executing args_test_program with arguments...");
        let result = execve("args_test_program", &args, &envs);
        if result < 0 {
            println!("Child: execve failed with error: {}", result);
            exit(1);
        }
        // Should not reach here if execve succeeds
        exit(0);
    } else {
        // Parent process
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
        println!("Parent: Child process exited with code: {}", exit_code);
    }

    // Test 2: execve with empty arguments
    println!("\n=== Test 2: execve with empty arguments ===");
    
    let pid = fork();
    if pid == 0 {
        // Child process
        let args: &[&str] = &[];
        let envs: &[&str] = &[];
        
        println!("Child: Executing args_test_program with no arguments...");
        let result = execve("args_test_program", &args, &envs);
        if result < 0 {
            println!("Child: execve failed with error: {}", result);
            exit(1);
        }
        exit(0);
    } else {
        // Parent process
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
        println!("Parent: Child process exited with code: {}", exit_code);
    }

    // Test 3: execve with non-existent program
    println!("\n=== Test 3: execve with non-existent program ===");
    
    let pid = fork();
    if pid == 0 {
        // Child process
        let args = ["nonexistent"];
        let envs = ["PATH=/bin"];
        
        println!("Child: Trying to execute non-existent program...");
        let result = execve("nonexistent_program", &args, &envs);
        if result < 0 {
            println!("Child: execve correctly failed with error: {}", result);
            exit(0);
        } else {
            println!("Child: execve should have failed but didn't");
            exit(1);
        }
    } else {
        // Parent process
        let mut exit_code = 0;
        wait_pid(pid as usize, &mut exit_code);
        println!("Parent: Child process exited with code: {}", exit_code);
    }

    println!("\nAll execve tests completed!");
    0
}