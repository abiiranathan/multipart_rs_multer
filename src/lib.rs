use futures::stream::once;
use mime::Mime;
use multer::bytes::Bytes;
use multer::Multipart;
use once_cell::sync::Lazy;
use std::convert::Infallible;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;
use tokio::runtime;

/// Represents a form data with fields and files.
#[repr(C)]
#[derive(Debug)]
pub struct FormData {
    fields: *mut FormField,    // Array of fields in the form data.
    field_count: usize,        // Number of fields in the form data.
    files: *mut MultipartFile, // Array of files in the form data.
    file_count: usize,         // Number of files in the form data.
}

/// Represents a file with filename, content type, content, and content length.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct MultipartFile {
    filename: *const c_char,     // Filename of the file.
    content_type: *const c_char, // Content type of the file.
    content: *mut u8,            // Raw bytes of the file content.
    content_length: usize,       // Length of the file content in bytes.
    field_name: *const c_char,   // Name of the field that the file is associated with.
}

/// Represents a field with name and value.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct FormField {
    name: *const c_char,
    value: *const c_char,
}

struct RuntimeManager {
    runtime: Option<runtime::Runtime>,
}

impl RuntimeManager {
    fn new() -> Self {
        RuntimeManager { runtime: None }
    }

    fn init(&mut self) {
        if self.runtime.is_none() {
            self.runtime = Some(
                runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create Tokio runtime"),
            );
        }
    }

    fn runtime(&self) -> &runtime::Runtime {
        self.runtime.as_ref().expect("Runtime not initialized")
    }

    fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        }
    }
}

// Lazy static Tokio runtime manager
static RUNTIME_MANAGER: Lazy<Mutex<RuntimeManager>> = Lazy::new(|| {
    let mut manager = RuntimeManager::new();
    manager.init(); // Initialize the Tokio runtime
    Mutex::new(manager)
});

impl Drop for RuntimeManager {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(5));
        }
        println!("Dropped RuntimeManager");
    }
}

fn rt() -> std::sync::MutexGuard<'static, RuntimeManager> {
    RUNTIME_MANAGER.lock().unwrap()
}

/// Parses the multipart form data from the given body.
/// Returns a pointer to the parsed form data. If the body is null, returns a pointer to an empty form data.
/// Likewise if the boundary is not found, returns a pointer to an empty form data that must be freed.
/// The caller is responsible for freeing the form data by calling `free_multipart_form_data`.
// #[no_mangle]
async fn rt_parse_multipart_form_data(body: *const c_char) -> *mut FormData {
    let default_form_data = FormData {
        fields: std::ptr::null_mut(),
        field_count: 0,
        files: std::ptr::null_mut(),
        file_count: 0,
    };

    if body.is_null() {
        return Box::into_raw(Box::new(default_form_data));
    }

    // Convert body to bytes as this C string may not contain valid UTF-8.
    let body = unsafe { CStr::from_ptr(body).to_bytes() };

    // Extract the boundary, find the first occurrence of '\r\n' in the body.
    let boundary_index = body.iter().position(|&b| b == b'\r').map(|index| index + 2);
    if boundary_index.is_none() {
        return Box::into_raw(Box::new(default_form_data));
    }

    // Convert the boundary index to a string slice.
    // We subtract 2 from the boundary index to exclude the '\r\n' characters.
    // We start from 2 to exclude the leading '--' characters.
    let boundary = std::str::from_utf8(&body[2..boundary_index.unwrap() - 2]).unwrap_or_default();

    if boundary.is_empty() {
        return Box::into_raw(Box::new(default_form_data));
    }

    // Initialize vectors to store form fields and files.
    let mut fields: Vec<FormField> = Vec::new();
    let mut files: Vec<MultipartFile> = Vec::new();

    let stream = once(async move { Result::<Bytes, Infallible>::Ok(Bytes::from(body)) });

    let mut multipart = Multipart::new(stream, boundary);
    let default_content_type = "application/octet-stream".parse::<Mime>().unwrap();

    // Iterate over the fields, `next_field` method will return the next field if
    // available.
    while let Some(mut field) = multipart.next_field().await.unwrap() {
        let name = String::from(field.name().unwrap());
        let is_file = field.file_name().is_some();

        let file_name = {
            if is_file {
                field.file_name().unwrap().to_string()
            } else {
                String::new()
            }
        };

        let content_type = field.content_type().unwrap_or(&default_content_type);

        if is_file {
            // Extract the file name, content type, and content length.
            let content_type = content_type.to_string();

            // Read the file content into a vector.
            let mut content: Vec<u8> = Vec::new();
            while let Some(chunk) = field.chunk().await.unwrap() {
                content.extend_from_slice(&chunk);
            }

            // Update the content length.
            let content_length = content.len();

            // Convert the file name, content type, and field name to C strings.
            let content_type = CString::new(content_type).unwrap().into_raw();
            let field_name = CString::new(name).unwrap().into_raw();
            let filename = CString::new(file_name).unwrap().into_raw();

            // Create a new file and push it to the files vector.
            let file = MultipartFile {
                filename,
                content_type,
                content: content.as_mut_ptr(),
                content_length,
                field_name,
            };

            files.push(file);

            // Prevent the vector from being deallocated.
            std::mem::forget(content);
        } else {
            // Extract the field value.
            let value = field.text().await.unwrap_or_default();

            // Convert the field name and value to C strings.
            let name = CString::new(name).unwrap().into_raw();
            let value = CString::new(value).unwrap().into_raw();

            // Create a new field and push it to the fields vector.
            let field = FormField { name, value };
            fields.push(field);
        }
    }

    // Convert vectors into boxed slices and get raw pointers.
    let fields_slice = fields.into_boxed_slice();
    let files_slice = files.into_boxed_slice();

    let form_data = FormData {
        fields: fields_slice.as_ptr() as *mut FormField,
        field_count: fields_slice.len() as usize,
        files: files_slice.as_ptr() as *mut MultipartFile,
        file_count: files_slice.len(),
    };

    // Prevent the boxed slices from being deallocated.
    std::mem::forget(fields_slice);
    std::mem::forget(files_slice);

    Box::into_raw(Box::new(form_data))
}

#[no_mangle]
pub extern "C" fn parse_multipart_form_data(body: *const c_char) -> *mut FormData {
    // Get the Tokio runtime manager
    let runtime = rt();
    runtime
        .runtime()
        .block_on(async { rt_parse_multipart_form_data(body).await })
}

/// Frees the given form data. If the form data is null, does nothing.
#[no_mangle]
pub extern "C" fn free_multipart_form_data(form_data: *mut FormData) {
    if form_data.is_null() {
        return;
    }

    let data = unsafe { Box::from_raw(form_data) };

    // Free the fields
    for i in 0..data.field_count {
        let field = unsafe { &*data.fields.add(i) };
        unsafe {
            let _ = CString::from_raw(field.name as *mut c_char);
            let _ = CString::from_raw(field.value as *mut c_char);
        }
    }

    // Free the fields array
    if !data.fields.is_null() {
        unsafe {
            let _ = Vec::from_raw_parts(data.fields, data.field_count, data.field_count);
        }
    }

    // Free the files
    for i in 0..data.file_count {
        let file = unsafe { &*data.files.add(i) };
        unsafe {
            let _ = CString::from_raw(file.filename as *mut c_char);
            let _ = CString::from_raw(file.content_type as *mut c_char);
            let _ = CString::from_raw(file.field_name as *mut c_char);
            let _ = Vec::from_raw_parts(file.content, file.content_length, file.content_length);
        }
    }

    // Free the files array
    if !data.files.is_null() {
        unsafe {
            let _ = Vec::from_raw_parts(data.files, data.file_count, data.file_count);
        }
    }
}

// Explicity shutdown the runtime when the library is unloaded.
#[no_mangle]
pub extern "C" fn shutdown_runtime() {
    rt().shutdown();
}
