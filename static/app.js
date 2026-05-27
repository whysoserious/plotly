const PREVIEW_N = 500;

const form = document.getElementById('upload-form');
const fileInput = document.getElementById('file-input');
const uploadStatus = document.getElementById('upload-status');
const svgDisplay = document.getElementById('svg-display');
const previewText = document.getElementById('preview-text');
const previewN = document.getElementById('preview-n');
const fileList = document.getElementById('file-list');

previewN.textContent = PREVIEW_N;

async function refreshFileList() {
    const res = await fetch('/api/files');
    if (!res.ok) {
        fileList.innerHTML = `<li>Error loading list: ${res.status}</li>`;
        return;
    }
    const files = await res.json();
    if (files.length === 0) {
        fileList.classList.add('empty');
        fileList.innerHTML = '<li>No files uploaded.</li>';
        return;
    }
    fileList.classList.remove('empty');
    fileList.innerHTML = files.map(f => `
        <li>
            <a href="#" data-id="${f.id}">${escapeHtml(f.filename)}</a>
            <span class="meta">${f.size} B · ${f.content_type}</span>
        </li>
    `).join('');
    fileList.querySelectorAll('a').forEach(a => {
        a.addEventListener('click', (e) => {
            e.preventDefault();
            showFile(a.dataset.id);
        });
    });
}

async function showFile(id) {
    svgDisplay.classList.remove('empty');
    svgDisplay.innerHTML = `<object data="/api/files/${id}/content" type="image/svg+xml"></object>`;

    const res = await fetch(`/api/files/${id}/preview?n=${PREVIEW_N}`);
    if (!res.ok) {
        previewText.textContent = `Error loading preview: ${res.status}`;
        return;
    }
    previewText.classList.remove('empty');
    previewText.textContent = await res.text();
}

function escapeHtml(s) {
    return s.replace(/[&<>"']/g, c => ({
        '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
    }[c]));
}

form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const file = fileInput.files[0];
    if (!file) return;

    const formData = new FormData();
    formData.append('file', file);

    uploadStatus.textContent = 'Uploading…';
    uploadStatus.className = 'status';

    const res = await fetch('/api/files', { method: 'POST', body: formData });

    if (!res.ok) {
        let detail = `HTTP ${res.status}`;
        try {
            const err = await res.json();
            if (err.detail) detail = err.detail;
        } catch (_) { /* not JSON */ }
        uploadStatus.textContent = `Error: ${detail}`;
        uploadStatus.classList.add('error');
        return;
    }

    const meta = await res.json();
    uploadStatus.textContent = `Uploaded: ${meta.filename} (${meta.size} B)`;
    uploadStatus.classList.add('ok');
    await refreshFileList();
    await showFile(meta.id);
});

refreshFileList();
