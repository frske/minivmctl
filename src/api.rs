use std::io;

use rocket::{
    Build, Rocket, State, delete, get,
    http::Status,
    post,
    response::status,
    routes,
    serde::{Serialize, json::Json},
};

use crate::vm_manager::{VmInfo, VmManagerHandle};

#[derive(Debug, Serialize)]
#[serde(crate = "rocket::serde")]
struct VmResponse {
    id: u8,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(crate = "rocket::serde")]
struct ApiMessage {
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(crate = "rocket::serde")]
struct ApiError {
    error: String,
}

type ApiResult<T> = Result<Json<T>, status::Custom<Json<ApiError>>>;

pub fn rocket(handle: VmManagerHandle) -> Rocket<Build> {
    rocket::build()
        .manage(handle)
        .mount("/", routes![list_vms, create_vm, delete_vm])
}

#[get("/vms")]
fn list_vms(handle: &State<VmManagerHandle>) -> ApiResult<Vec<VmResponse>> {
    handle
        .list_vms()
        .map(|vms| Json(vms.into_iter().map(VmResponse::from).collect()))
        .map_err(api_error)
}

#[post("/vms")]
fn create_vm(handle: &State<VmManagerHandle>) -> ApiResult<VmResponse> {
    let vm = handle.start_vm().map_err(api_error)?;

    Ok(Json(VmResponse::from(vm)))
}

#[delete("/vms/<id>")]
fn delete_vm(handle: &State<VmManagerHandle>, id: u8) -> ApiResult<ApiMessage> {
    handle.request_stop_vm(id).map_err(api_error)?;

    Ok(Json(ApiMessage {
        message: format!("vm {id} stop requested"),
    }))
}

impl From<VmInfo> for VmResponse {
    fn from(vm: VmInfo) -> Self {
        Self {
            id: vm.id,
            status: vm.status,
        }
    }
}

fn api_error(err: Box<dyn std::error::Error + Send + Sync>) -> status::Custom<Json<ApiError>> {
    let status = err
        .downcast_ref::<io::Error>()
        .map(io_status)
        .unwrap_or(Status::InternalServerError);

    status::Custom(
        status,
        Json(ApiError {
            error: err.to_string(),
        }),
    )
}

fn io_status(err: &io::Error) -> Status {
    match err.kind() {
        io::ErrorKind::InvalidInput => Status::BadRequest,
        io::ErrorKind::NotFound => Status::NotFound,
        io::ErrorKind::AlreadyExists => Status::Conflict,
        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe => Status::ServiceUnavailable,
        io::ErrorKind::TimedOut => Status::GatewayTimeout,
        _ => Status::InternalServerError,
    }
}
