document.getElementById('login').addEventListener('submit', async (e) => {
  e.preventDefault();
  const msg = document.getElementById('msg');
  msg.textContent = '';
  const btn = e.target.querySelector('button');
  btn.disabled = true;
  try {
    const r = await fetch('/api/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-ezc': '1' },
      body: JSON.stringify({
        username: document.getElementById('u').value,
        password: document.getElementById('p').value,
      }),
    });
    if (r.ok) location.href = '/';
    else msg.textContent = await r.text();
  } catch (err) {
    msg.textContent = '网络错误: ' + err;
  } finally {
    btn.disabled = false;
  }
});
