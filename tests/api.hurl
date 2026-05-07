# Run with:
# hurl --variable base_url=http://127.0.0.1:8000 \
#   tests/api.hurl

GET {{base_url}}/vms
HTTP 200
[Asserts]
jsonpath "$" isCollection

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "web-1",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0"
}
HTTP 200
[Captures]
vm_1_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "web-1"
jsonpath "$.ip_address" == "10.10.0.2"
jsonpath "$.status" == "running"

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "worker-1",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0"
}
HTTP 200
[Captures]
vm_2_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "worker-1"
jsonpath "$.ip_address" == "10.10.0.3"
jsonpath "$.status" == "running"

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "worker-2",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0"
}
HTTP 200
[Captures]
vm_3_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "worker-2"
jsonpath "$.ip_address" == "10.10.0.4"
jsonpath "$.status" == "running"

GET {{base_url}}/vms
HTTP 200
[Asserts]
jsonpath "$[0].id" == {{vm_1_id}}
jsonpath "$[0].name" == "web-1"
jsonpath "$[0].ip_address" == "10.10.0.2"
jsonpath "$[0].status" == "running"
jsonpath "$[1].id" == {{vm_2_id}}
jsonpath "$[1].name" == "worker-1"
jsonpath "$[1].ip_address" == "10.10.0.3"
jsonpath "$[1].status" == "running"
jsonpath "$[2].id" == {{vm_3_id}}
jsonpath "$[2].name" == "worker-2"
jsonpath "$[2].ip_address" == "10.10.0.4"
jsonpath "$[2].status" == "running"

DELETE {{base_url}}/vms/{{vm_2_id}}
HTTP 200
[Asserts]
jsonpath "$.message" contains "stop requested"
