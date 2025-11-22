use std::fs;
use std::os::unix::net::UnixListener;

#[test]
fn test_socket_validation_nonexistent() {
    let socket_path = "/tmp/test_nonexistent_socket_12345.sock";
    let _ = fs::remove_file(socket_path);
    assert!(!std::path::Path::new(socket_path).exists());
}

#[test]
fn test_socket_validation_regular_file() {
    let test_dir = "/tmp/socket_validation_tests";
    fs::create_dir_all(test_dir).unwrap();
    let file_path = format!("{}/regular_file.txt", test_dir);

    fs::write(&file_path, "not a socket").unwrap();

    let metadata = fs::metadata(&file_path).unwrap();
    use std::os::unix::fs::FileTypeExt;
    assert!(!metadata.file_type().is_socket());

    fs::remove_file(&file_path).ok();
    fs::remove_dir(test_dir).ok();
}

#[test]
fn test_socket_validation_real_socket() {
    let test_dir = "/tmp/socket_validation_tests_real";
    fs::create_dir_all(test_dir).unwrap();
    let socket_path = format!("{}/real_socket.sock", test_dir);

    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();

    let metadata = fs::metadata(&socket_path).unwrap();
    use std::os::unix::fs::FileTypeExt;
    assert!(metadata.file_type().is_socket());

    drop(listener);
    fs::remove_file(&socket_path).ok();
    fs::remove_dir(test_dir).ok();
}
