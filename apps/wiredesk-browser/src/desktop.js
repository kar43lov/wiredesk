// Desktop requests use the same owner key; no remote shell or local storage.
let desktopController, desktopBusy = false, desktopReady = false;
let desktopQueue = Promise.resolve(), desktopPoints = [], desktopButton = 0;
let desktopInputPending = 0;
let desktopStartPoint;
const desktopMode = () => $('mode').value === 'desktop';
const desktopHandlers = {};
for (const event of ['onpointerdown','onpointermove','onpointerup','onpointercancel','onkeydown','onpaste']) desktopHandlers[event] = $('screen')[event];
const desktopType = $('type').onclick;
function desktopPoint(e) {
  const r = $('screen').getBoundingClientRect();
  return [Math.round(Math.max(0,Math.min(65535,(e.clientX-r.left)/r.width*65535))),Math.round(Math.max(0,Math.min(65535,(e.clientY-r.top)/r.height*65535)))];
}
async function desktopFrame() {
  if (!desktopMode() || desktopBusy || !token()) return;
  const epoch = viewerEpoch, started = performance.now(); desktopBusy = true;
  desktopController = new AbortController();
  try {
    const r = await fetch('/desktop/frame', {headers:{Authorization:'Bearer '+token()}, signal:desktopController.signal});
    if (!r.ok) throw Error(r.status === 412 ? 'Обновите хост Windows для режима экрана' : 'Экран Windows пока недоступен');
    const body = await r.blob(); if (body.size > 2*1024*1024) throw Error('Слишком большой кадр');
    const url = URL.createObjectURL(body);
    try {
      const img = new Image();
      await new Promise((resolve,reject) => {img.onload=resolve;img.onerror=()=>reject(Error('Повреждённый кадр'));img.src=url;});
      if (epoch !== viewerEpoch || !desktopMode()) return;
      const previous = $('screen').dataset.objectUrl;
      $('screen').src=url; $('screen').dataset.objectUrl=url; desktopReady=true;
      if (previous) URL.revokeObjectURL(previous);
      status('Экран Windows · кадр за '+((performance.now()-started)/1000).toFixed(2)+' с');
    } finally {if ($('screen').dataset.objectUrl !== url) URL.revokeObjectURL(url);}
  } catch(e) {if (epoch === viewerEpoch && e.name !== 'AbortError') {desktopReady=false;status(e.message);}}
  finally {desktopBusy=false;desktopController=undefined;}
}
function desktopAction(action) {
  if (!desktopMode() || !desktopReady) return;
  if (desktopInputPending >= 32) {status('Ввод занят: дождитесь передачи предыдущих действий');return;}
  desktopInputPending++;
  const epoch = viewerEpoch;
  desktopQueue = desktopQueue.then(async () => {
    if (epoch !== viewerEpoch || !desktopMode()) return;
    const r = await fetch('/desktop/input',{method:'POST',headers:{Authorization:'Bearer '+token(),'Content-Type':'application/json'},body:JSON.stringify(action)});
    if (!r.ok) throw Error('Не удалось передать ввод в Windows');
    if (epoch === viewerEpoch) desktopFrame();
  }).catch(e => {if(epoch === viewerEpoch) status(e.message);}).finally(()=>{desktopInputPending--;});
}
$('mode').onchange = () => {
  viewerEpoch++; clearTimeout(reconnectTimer); clearTimeout(hoverTimer);
  desktopController?.abort(); desktopReady=false; desktopPoints=[]; nextMove=undefined; pressed=false;
  if(ws) {ws.onclose=null;ws.close();ws=undefined;disconnected();}
  const old=$('screen').dataset.objectUrl; if(old) URL.revokeObjectURL(old);delete $('screen').dataset.objectUrl;$('screen').removeAttribute('src');
  for(const id of ['tabs','refresh','back','forward','url','go']) $(id).hidden=desktopMode();
  $('screen').alt=desktopMode()?'Экран Windows':'Удалённая вкладка';
  if(desktopMode()) { $('auto').checked=true;status('Подключение к экрану Windows…');desktopFrame(); } else {$('auto').checked=false;tabs();}
};
$('connect').onclick = () => desktopMode()?desktopFrame():tabs();
$('frame').onclick = () => desktopMode()?desktopFrame():screenshot();
$('type').onclick = () => desktopMode()?desktopAction({kind:'text',text:$('text').value}):desktopType();
$('screen').onpointerdown = e => {
  if(!desktopMode()) return desktopHandlers.onpointerdown(e);
  if(!desktopReady || ![0,1,2].includes(e.button)) return;
  e.preventDefault();$('screen').focus();$('screen').setPointerCapture(e.pointerId);desktopButton=e.button===2?1:e.button===1?2:0;desktopPoints=[desktopPoint(e)];desktopStartPoint=[e.clientX,e.clientY];
};
$('screen').onpointermove = e => {
  if(!desktopMode()) return desktopHandlers.onpointermove(e);
  if(desktopPoints.length && desktopButton===0 && desktopPoints.length<63) desktopPoints.push(desktopPoint(e));
};
$('screen').onpointerup = e => {
  if(!desktopMode()) return desktopHandlers.onpointerup(e);
  if(!desktopPoints.length) return;
  const last=desktopPoint(e),points=desktopPoints;desktopPoints=[];
  if(desktopButton===0 && Math.hypot(e.clientX-desktopStartPoint[0],e.clientY-desktopStartPoint[1])>8) desktopAction({kind:'drag',points:[...points,last]});
  else desktopAction({kind:'click',x:last[0],y:last[1],button:desktopButton});
};
$('screen').onpointercancel = e => {if(!desktopMode()) return desktopHandlers.onpointercancel(e);desktopPoints=[];};
$('screen').addEventListener('wheel',e=>{
  if(!desktopMode() || !desktopReady) return;e.preventDefault();const [x,y]=desktopPoint(e);
  desktopAction({kind:'wheel',x,y,delta:-Math.sign(e.deltaY)*120});
},{passive:false});
$('screen').onpaste = e => {
  if(!desktopMode()) return desktopHandlers.onpaste(e);
  if(!desktopReady) return;e.preventDefault();const text=e.clipboardData?.getData('text/plain');if(text) desktopAction({kind:'text',text});
};
$('screen').onkeydown = e => {
  if(!desktopMode()) return desktopHandlers.onkeydown(e);
  if(!desktopReady) return;
  if((e.ctrlKey||e.metaKey)&&e.key.toLowerCase()==='v') return;
  e.preventDefault();if(['Control','Alt','Shift','Meta'].includes(e.key)) return;
  if(e.key.length===1 && !e.ctrlKey && !e.metaKey && !e.altKey) desktopAction({kind:'text',text:e.key});
  else desktopAction({kind:'key',vk:e.keyCode,modifiers:(e.altKey?1:0)|((e.ctrlKey||e.metaKey)?2:0)|(e.shiftKey?4:0)});
};
setInterval(()=>{if(desktopMode() && $('auto').checked && !document.hidden) desktopFrame();},1000);
