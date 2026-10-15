/* Familiar Smith navigation. Existing controllers still own every action. */
'use strict';
(() => {
 const el=id=>document.getElementById(id),app=document.querySelector('.app');
 const icons={chat:'M4 4h16v12H9l-5 4Z',models:'M7 7h10v10H7Z M9 2v5 M15 2v5 M9 17v5 M15 17v5 M2 9h5 M2 15h5 M17 9h5 M17 15h5',agents:'M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2 M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8 M22 21v-2a4 4 0 0 0-3-3.87 M16 3.13a4 4 0 0 1 0 7.75',activity:'M2 12h4l3-8 6 16 3-8h4',settings:'M4 21v-7 M4 10V3 M12 21v-9 M12 8V3 M20 21v-5 M20 12V3 M1 14h6 M9 8h6 M17 16h6'};
 const rail=document.createElement('nav');rail.className='smith-rail';rail.setAttribute('aria-label','Smith');
 const brand=document.createElement('div');brand.className='rail-brand';brand.setAttribute('aria-label','Smith');brand.innerHTML=el('sidebar').querySelector('.brand-mark').innerHTML;rail.append(brand);
 const labels={chat:'Chat',models:'Models',agents:'Agents & Autonomy',activity:'Activity',settings:'Settings'};
 const railButtons={};
 for(const [key,label]of Object.entries(labels)){
  if(key==='settings'){const spacer=document.createElement('div');spacer.className='rail-spacer';rail.append(spacer)}
  const b=document.createElement('button');b.type='button';b.id='rail-'+key;b.title=label;b.setAttribute('aria-label',label);b.innerHTML='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="'+icons[key]+'"/></svg>';rail.append(b);railButtons[key]=b;
 }
 app.prepend(rail);
 const title=el('sidebar').querySelector('.brand>span:last-of-type');title.textContent='Chats';
 const welcomeMark=document.createElement('div');welcomeMark.className='welcome-mark';welcomeMark.setAttribute('aria-hidden','true');welcomeMark.innerHTML=brand.innerHTML;el('welcome').prepend(welcomeMark);
 document.querySelector('.top-actions').prepend(el('model-select'));
 const activity=document.createElement('dialog');activity.id='activity';activity.className='workspace-screen';activity.setAttribute('aria-labelledby','activity-title');activity.innerHTML='<div class="dialog-head"><div><h2 id="activity-title">Activity</h2><p>Runs, saved work and imported history.</p></div><button type="button" class="quiet" data-close="activity" aria-label="Back to chat">×</button></div><div class="dialog-body"><div class="activity-rows" id="activity-rows"></div></div>';document.body.append(activity);
 const destinations=[['Runs','Tasks and their saved activity.','file-tasks-button'],['Artifacts','Saved responses and generated files.','artifacts-button'],['Documents','Original files and reviewed conversation context.','documents-button'],['Imported records','Original history retained by an explicit profile import.','legacy-records-button'],['Trash','Recover deleted conversations.','open-trash']];
 for(const [name,description,id]of destinations){const row=document.createElement('div');row.className='activity-row';const text=document.createElement('div'),strong=document.createElement('strong'),p=document.createElement('p'),button=document.createElement('button');strong.textContent=name;p.textContent=description;text.append(strong,p);button.textContent='Open';button.setAttribute('aria-label','Open '+name);button.onclick=()=>el(id)?.click();row.append(text,button);el('activity-rows').append(row)}
 activity.querySelector('[data-close]').onclick=()=>activity.close();
 const models=document.createElement('dialog');models.id='models';models.className='workspace-screen';models.setAttribute('aria-labelledby','models-title');models.innerHTML='<div class="dialog-head"><div><h2 id="models-title">Models</h2><p>Choose a configured model or manage a local runtime.</p></div><button type="button" class="quiet" aria-label="Back to chat">×</button></div><div class="dialog-body"><p id="models-selected"></p><div class="activity-rows"><div class="activity-row"><div><strong>Connection and model</strong><p>Use the tested model list and local runtime controls.</p></div><button type="button" id="models-connection">Configure</button></div></div><p class="intro" style="font-size:12px;margin-top:16px">The original download catalog has not been connected in this build.</p></div>';document.body.append(models);models.querySelector('.dialog-head>button').onclick=()=>models.close();el('models-connection').onclick=()=>el('connection-button').click();
 const screens={settings:'settings','file-tasks':'agents',activity:'activity',models:'models',artifacts:'activity',documents:'activity','legacy-records':'activity',trash:'activity'};
 function reflect(){if(profileClosed)return;const open=Object.keys(screens).filter(id=>el(id)?.open).at(-1),view=open?screens[open]:'chat';app.dataset.workspaceView=view;for(const [name,b]of Object.entries(railButtons)){if(name===view)b.setAttribute('aria-current','page');else b.removeAttribute('aria-current')}rail.inert=el('sidebar').classList.contains('open');}
 function dismiss(except){for(const id of Object.keys(screens)){const d=el(id);if(d?.open&&d!==except)d.close()}}
 for(const id of Object.keys(screens)){const dialog=el(id);if(!dialog)continue;dialog.classList.add('workspace-screen');dialog.dataset.desktopScreen='true';dialog.addEventListener('close',reflect);dialog.addEventListener('cancel',reflect)}
 window.SmithNavigation?.registerViews({activity:'activity',models:'models','legacy-records':'imported'});
 window.addEventListener('smith-workspace-changed',reflect);
 document.addEventListener('keydown',event=>{if(event.key!=='Escape'||event.defaultPrevented||document.querySelector('dialog:modal'))return;const open=Object.keys(screens).map(el).filter(d=>d?.open).at(-1);if(open){event.preventDefault();open.close();railButtons.chat.focus();window.SmithNavigation?.record()}});
 railButtons.chat.onclick=()=>{dismiss();closeNavigation();reflect();window.SmithNavigation?.record();el('prompt').focus()};
 railButtons.settings.onclick=()=>{closeNavigation();el('connection-button').click()};
 railButtons.agents.onclick=()=>{closeNavigation();el('file-tasks-button').click()};
 railButtons.activity.onclick=()=>{closeNavigation();activity.showModal()};
 railButtons.models.onclick=()=>{closeNavigation();el('models-selected').textContent='Selected: '+(config.model||'No model')+' · '+(config.provider==='mock'?'Demo fixture':config.provider);models.showModal()};
 new MutationObserver(reflect).observe(el('sidebar'),{attributes:true,attributeFilter:['class']});
 window.addEventListener('smith-profile-closed',()=>{for(const d of document.querySelectorAll('dialog'))d.close()});
 window.addEventListener('smith-app-ready',reflect);reflect();
})();
