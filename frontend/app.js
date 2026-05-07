const API_BASE = "http://127.0.0.1:8000";
const SSH_KEY_STORAGE_KEY = "scratch.vm.sshPublicKey";

const form = document.querySelector("#create-form");
const nameInput = document.querySelector("#name");
const sshKeyInput = document.querySelector("#ssh-key");
const message = document.querySelector("#message");
const refreshButton = document.querySelector("#refresh");
const createButton = document.querySelector("#create");
const vmList = document.querySelector("#vm-list");

sshKeyInput.value = localStorage.getItem(SSH_KEY_STORAGE_KEY) ?? "";

async function api(path, options = {}) {
  let response;
  try {
    response = await fetch(`${API_BASE}${path}`, {
      headers: { "Content-Type": "application/json", ...(options.headers ?? {}) },
      ...options,
    });
  } catch (error) {
    throw new Error(`Cannot reach API at ${API_BASE}. Check backend is running and CORS is allowed.`);
  }

  const body = await readJson(response);
  if (!response.ok) {
    throw new Error(body?.error ?? `request failed with ${response.status}`);
  }

  return body;
}

async function readJson(response) {
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}

function setBusy(isBusy) {
  refreshButton.disabled = isBusy;
  createButton.disabled = isBusy;
}

function setMessage(text, kind = "") {
  message.textContent = text;
  message.className = `message ${kind}`.trim();
}

function renderVms(vms) {
  if (vms.length === 0) {
    vmList.innerHTML = '<tr><td colspan="5" class="empty">No VMs running</td></tr>';
    return;
  }

  vmList.replaceChildren(
    ...vms.map((vm) => {
      const row = document.createElement("tr");
      row.innerHTML = `
        <td>${vm.id}</td>
        <td>${escapeHtml(vm.name)}</td>
        <td>
          <button class="copy-ip" type="button" data-ip="${escapeHtml(vm.ip_address)}" title="Copy IP address">
            <span class="copy-ip-text">${escapeHtml(vm.ip_address)}</span>
            <svg class="copy-symbol" aria-hidden="true" viewBox="0 0 24 24">
              <path d="M8 8V5.75C8 4.78 8.78 4 9.75 4h8.5C19.22 4 20 4.78 20 5.75v8.5c0 .97-.78 1.75-1.75 1.75H16" />
              <path d="M5.75 8h8.5c.97 0 1.75.78 1.75 1.75v8.5c0 .97-.78 1.75-1.75 1.75h-8.5C4.78 20 4 19.22 4 18.25v-8.5C4 8.78 4.78 8 5.75 8Z" />
            </svg>
          </button>
        </td>
        <td><span class="status">${escapeHtml(vm.status)}</span></td>
        <td><button class="danger" type="button" data-id="${vm.id}">Delete</button></td>
      `;
      return row;
    }),
  );
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"]/g, (char) => {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[char];
  });
}

async function loadVms() {
  setBusy(true);
  try {
    renderVms(await api("/vms"));
    setMessage("VM list refreshed.", "ok");
  } catch (error) {
    setMessage(error.message, "error");
  } finally {
    setBusy(false);
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  setBusy(true);
  const sshKey = sshKeyInput.value.trim();

  try {
    localStorage.setItem(SSH_KEY_STORAGE_KEY, sshKey);
    const vm = await api("/vms", {
      method: "POST",
      body: JSON.stringify({
        name: nameInput.value.trim(),
        ssh_key: sshKey,
      }),
    });
    nameInput.value = "";
    sshKeyInput.value = sshKey;
    setMessage(`VM ${vm.id} created.`, "ok");
    await loadVms();
  } catch (error) {
    setMessage(error.message, "error");
  } finally {
    setBusy(false);
  }
});

sshKeyInput.addEventListener("input", () => {
  localStorage.setItem(SSH_KEY_STORAGE_KEY, sshKeyInput.value.trim());
});

refreshButton.addEventListener("click", loadVms);

vmList.addEventListener("click", async (event) => {
  const copyButton = event.target.closest("button[data-ip]");
  if (copyButton) {
    try {
      await copyText(copyButton.dataset.ip);
      animateCopy(copyButton);
      setMessage(`Copied ${copyButton.dataset.ip}.`, "ok");
    } catch (error) {
      setMessage("Could not copy IP address.", "error");
    }
    return;
  }

  const button = event.target.closest("button[data-id]");
  if (!button) return;

  setBusy(true);
  try {
    await api(`/vms/${button.dataset.id}`, { method: "DELETE" });
    setMessage(`VM ${button.dataset.id} stop requested.`, "ok");
    await loadVms();
  } catch (error) {
    setMessage(error.message, "error");
  } finally {
    setBusy(false);
  }
});

function animateCopy(button) {
  const text = button.querySelector(".copy-ip-text");
  button.classList.remove("copied");
  void button.offsetWidth;
  button.classList.add("copied");
  if (text?.animate) {
    text.animate(
      [
        { transform: "perspective(600px) translateY(0) rotateX(0deg)", color: "var(--text)" },
        { transform: "perspective(600px) translateY(-0.5px) rotateX(10deg)", color: "var(--success)" },
        { transform: "perspective(600px) translateY(0) rotateX(0deg)", color: "var(--success)" },
      ],
      {
        duration: 420,
        easing: "ease-out",
      },
    );
  }
  window.setTimeout(() => button.classList.remove("copied"), 420);
}

async function copyText(text) {
  if (navigator.clipboard) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.style.position = "fixed";
  textarea.style.left = "-9999px";
  document.body.append(textarea);
  textarea.select();

  try {
    if (!document.execCommand("copy")) {
      throw new Error("copy command failed");
    }
  } finally {
    textarea.remove();
  }
}

loadVms();
