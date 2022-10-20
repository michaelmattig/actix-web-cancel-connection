use std::any::Any;
use std::num::NonZeroI32;
use std::os::unix::prelude::AsRawFd;

use actix_http::Extensions;
use actix_rt::net::TcpStream;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use futures::stream::{AbortHandle, Abortable};
use nix::errno::Errno;
use nix::sys::socket::MsgFlags;
use std::os::unix::io::RawFd;
use tokio::task::JoinHandle;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    futures::try_join!(start_server(), poll_server())?;

    Ok(())
}

async fn start_server() -> std::io::Result<()> {
    HttpServer::new(|| App::new().route("/test", web::get().to(test_route)))
        .on_connect(connection_init)
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}

pub struct SocketFd(pub RawFd);

// during `on_connect` save the socket fd in the `conn_data`
pub fn connection_init(connection: &dyn Any, data: &mut Extensions) {
    if let Some(sock) = connection.downcast_ref::<TcpStream>() {
        let fd = sock.as_raw_fd();
        if let Ok(fd) = NonZeroI32::try_from(fd) {
            let fd = RawFd::from(fd);
            data.insert(SocketFd(fd));
        }
    }
}

// a test route that should abort work if the connection is cloed
async fn test_route(req: HttpRequest) -> impl Responder {
    let work = tokio::time::sleep(std::time::Duration::from_secs(6));

    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let abortable = Abortable::new(work, abort_registration);

    connection_closed(&req).map(|c| {
        tokio::task::spawn(async move {
            if c.await.is_ok() {
                eprintln!("connection closed, aborting work");
                abort_handle.abort();
            }
        })
    });

    let result = abortable.await;

    eprintln!("finished, work complete: {}", result.is_ok());
    HttpResponse::Ok()
}

// return a handle that joins when the connection is closed
pub fn connection_closed(req: &HttpRequest) -> Option<JoinHandle<()>> {
    eprintln!("Monitoring req conn");
    req.conn_data::<SocketFd>().map(|fd| {
        let fd = fd.0;
        tokio::task::spawn(async move {
            let mut data = vec![];
            loop {
                let r = nix::sys::socket::recv(fd, data.as_mut_slice(), MsgFlags::MSG_PEEK);

                match r {
                    Ok(0) | Err(Errno::EBADF) => {
                        eprintln!("A connection closed");
                        return;
                    }
                    _ => (),
                }

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    })
}

// query the server with different request timeouts
async fn poll_server() -> std::io::Result<()> {
    // wait for server to start
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let client = reqwest::Client::new();

    for timeout_seconds in [3, 60] {
        eprintln!("Timeout: {}", timeout_seconds);
        let request = client
            .get("http://127.0.0.1:8080/test")
            .timeout(std::time::Duration::from_secs(timeout_seconds))
            .send();

        let response = request.await;

        match response {
            Err(error) if error.is_timeout() => {
                eprintln!("Client: request timed out");
            }
            Err(error) => {
                eprintln!("error: {:?}", error);
            }
            Ok(response) => {
                let body = response.text().await;
                eprintln!("body: {:?}", body);
            }
        }

        // wait before next request
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        eprintln!();
    }

    Ok(())
}
