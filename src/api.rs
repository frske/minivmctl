use std::io;

use rocket::{
    Build, Request, Response, Rocket, State, delete,
    fairing::{Fairing, Info, Kind},
    get,
    http::{Header, Status},
    options, post,
    response::status,
    routes,
    serde::{Deserialize, Serialize, json::Json},
};
use tracing::{debug, info, warn};

use crate::vm_manager::{VmInfo, VmManagerHandle};

const ALLOWED_ORIGINS: &[&str] = &[
    "http://127.0.0.1:5173",
    "http://localhost:5173",
    "http://0.0.0.0:5173",
];

#[derive(Debug, Serialize)]
#[serde(crate = "rocket::serde")]
struct VmResponse {
    id: u8,
    name: String,
    ip_address: String,
    status: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "rocket::serde")]
struct CreateVmRequest {
    name: String,
    ssh_key: String,
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
        .attach(Cors)
        .mount("/", routes![list_vms, create_vm, delete_vm, options])
}

#[get("/vms")]
fn list_vms(handle: &State<VmManagerHandle>) -> ApiResult<Vec<VmResponse>> {
    debug!("list vms requested");
    let vms = handle.list_vms().map_err(api_error)?;
    let responses: Vec<VmResponse> = vms.into_iter().map(VmResponse::from).collect();
    Ok(Json(responses))
}

#[post("/vms", data = "<request>")]
fn create_vm(
    handle: &State<VmManagerHandle>,
    request: Json<CreateVmRequest>,
) -> ApiResult<VmResponse> {
    info!(name = %request.name, "create vm requested");
    let vm = handle
        .start_vm(request.name.clone(), request.ssh_key.clone())
        .map_err(api_error)?;

    Ok(Json(VmResponse::from(vm)))
}

#[delete("/vms/<id>")]
fn delete_vm(handle: &State<VmManagerHandle>, id: u8) -> ApiResult<ApiMessage> {
    info!(id, "delete vm requested");
    handle.request_stop_vm(id).map_err(api_error)?;

    Ok(Json(ApiMessage {
        message: format!("vm {id} stop requested"),
    }))
}

#[options("/<_path..>")]
fn options(_path: std::path::PathBuf) -> Status {
    Status::NoContent
}

struct Cors;

#[rocket::async_trait]
impl Fairing for Cors {
    fn info(&self) -> Info {
        Info {
            name: "Frontend CORS",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let origin = match request.headers().get_one("Origin") {
            Some(origin) if is_allowed_origin(origin) => origin,
            _ => {
                set_cors_headers(response, None);
                return;
            }
        };

        set_cors_headers(response, Some(origin));
    }
}

fn set_cors_headers<'a>(response: &mut Response<'a>, origin: Option<&'a str>) {
    if let Some(origin) = origin {
        response.set_header(Header::new("Access-Control-Allow-Origin", origin));
    }
    response.set_header(Header::new(
        "Access-Control-Allow-Methods",
        "GET, POST, DELETE, OPTIONS",
    ));
    response.set_header(Header::new("Access-Control-Allow-Headers", "Content-Type"));
    response.set_header(Header::new("Vary", "Origin"));
}

fn is_allowed_origin(origin: &str) -> bool {
    ALLOWED_ORIGINS.contains(&origin)
}

impl From<VmInfo> for VmResponse {
    fn from(vm: VmInfo) -> Self {
        Self {
            id: vm.id,
            name: vm.name,
            ip_address: vm.ip_address,
            status: vm.status,
        }
    }
}

fn api_error(err: Box<dyn std::error::Error + Send + Sync>) -> status::Custom<Json<ApiError>> {
    let status = err
        .downcast_ref::<io::Error>()
        .map(io_status)
        .unwrap_or(Status::InternalServerError);

    if status.code >= 500 {
        warn!(status = %status, error = %err, "api request failed");
    } else {
        debug!(status = %status, error = %err, "api request failed");
    }

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
