# Run with:
# hurl --variable base_url=https://127.0.0.1:8443 --insecure \
#   tests/api.hurl

GET {{base_url}}/vms
HTTP 200
[Asserts]
jsonpath "$" isCollection

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "web-1",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0",
  "memory_mib": 512
}
HTTP 201
[Captures]
vm_1_id: jsonpath "$.id"
[Asserts]
header "Location" == "/vms/{{vm_1_id}}"
jsonpath "$.name" == "web-1"
jsonpath "$.id" exists
jsonpath "$.ip_address" exists
jsonpath "$.memory_mib" == 512
jsonpath "$.status" == "running"

GET {{base_url}}/vms/{{vm_1_id}}
HTTP 200
[Asserts]
jsonpath "$.name" == "web-1"
jsonpath "$.id" == {{vm_1_id}}
jsonpath "$.ip_address" exists
jsonpath "$.memory_mib" == 512
jsonpath "$.status" == "running"

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "worker-1",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0",
  "memory_mib": 1024
}
HTTP 201
[Captures]
vm_2_id: jsonpath "$.id"
[Asserts]
header "Location" == "/vms/{{vm_2_id}}"
jsonpath "$.name" == "worker-1"
jsonpath "$.id" exists
jsonpath "$.ip_address" exists
jsonpath "$.memory_mib" == 1024
jsonpath "$.status" == "running"

POST {{base_url}}/vms
Content-Type: application/json
{
  "name": "worker-2",
  "ssh_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA56S5GP6miraNlvrXNO6cfe9nhMaGpzXVspQmmC/R9K frsk@dhcp-10-126-119-0",
  "memory_mib": 2048
}
HTTP 201
[Captures]
vm_3_id: jsonpath "$.id"
[Asserts]
header "Location" == "/vms/{{vm_3_id}}"
jsonpath "$.name" == "worker-2"
jsonpath "$.id" exists
jsonpath "$.ip_address" exists
jsonpath "$.memory_mib" == 2048
jsonpath "$.status" == "running"

GET {{base_url}}/vms
HTTP 200
[Asserts]
jsonpath "$" isCollection

DELETE {{base_url}}/vms/{{vm_1_id}}
HTTP 202
[Asserts]
jsonpath "$.message" contains "stop requested"

DELETE {{base_url}}/vms/{{vm_2_id}}
HTTP 202
[Asserts]
jsonpath "$.message" contains "stop requested"

DELETE {{base_url}}/vms/{{vm_3_id}}
HTTP 202
[Asserts]
jsonpath "$.message" contains "stop requested"
