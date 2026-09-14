// Render design targets, never production-acceptance baselines.
// Uses an explicitly supplied existing Playwright installation; installs nothing.
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';
import { createHash } from 'node:crypto';
const root=path.dirname(fileURLToPath(import.meta.url));
const arg=name=>{const i=process.argv.indexOf(name);return i<0?undefined:process.argv[i+1]};
const modulePath=arg('--playwright');
if(!modulePath)throw new Error('Pass --playwright /absolute/path/to/an/existing/playwright/index.js');
const {chromium}=createRequire(import.meta.url)(path.resolve(modulePath));
const browserPath=arg('--browser')||'/usr/bin/chromium';
const profile=await fs.mkdtemp(path.join(os.tmpdir(),'devmanager-ux-reference-'));
const cases=[["REF-STEERING", "steering", "step4-steering.png", 1440, 900], ["REF-LINEAGE", "lineage", "step4-lineage.png", 1440, 900], ["REF-SOURCE-MISSING", "source-missing", "step4-source-missing.png", 1440, 900], ["REF-EFFECTIVE-ACCESS", "effective-access", "step4-effective-access.png", 1440, 900], ["REF-CHECK-COVERAGE", "check-coverage", "step4-check-coverage.png", 1440, 900], ["REF-CONTEXT-DRIFT", "context-drift", "step4-context-drift.png", 1440, 900], ["REF-KNOWLEDGE-STATES", "knowledge-states", "step4-knowledge-states.png", 1440, 900], ["REF-UNCERTAIN-EFFECT", "uncertain-effect", "step4-uncertain-effect.png", 1440, 900], ["REF-DELIVERY-PENDING", "delivery-pending", "step4-delivery-pending.png", 1440, 900], ["REF-ACCESS-NARROW", "effective-access", "step4-access-narrow.png", 960, 720]];
const errors=[],requests=[],checks=[],captures=[];
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
function assert(value,message){if(!value)throw new Error(message);checks.push(message)}
async function processSnapshot(){
 const result=[];
 for(const name of await fs.readdir('/proc')){
  if(!/^\d+$/.test(name))continue;
  try{const stat=await fs.readFile(`/proc/${name}/stat`,'utf8');const fields=stat.slice(stat.lastIndexOf(')')+2).trim().split(/\s+/);result.push({pid:Number(name),ppid:Number(fields[1]),start:fields[19],command:(await fs.readFile(`/proc/${name}/cmdline`,'utf8')).replaceAll('\0',' ')});}catch{}
 }
 return result;
}
let context,owned=[],manifest;
try{
 context=await chromium.launchPersistentContext(profile,{executablePath:browserPath,headless:true,viewport:{width:1440,height:900},deviceScaleFactor:1,colorScheme:'dark',reducedMotion:'reduce',args:['--disable-background-networking']});
 const page=context.pages()[0]||await context.newPage();
 page.on('pageerror',e=>errors.push(String(e)));
 page.on('request',r=>{if(/^https?:/.test(r.url()))requests.push(r.url())});
 await page.route(/^https?:/,route=>route.abort());
 const url=pathToFileURL(path.join(root,'step4-reference.html')).href;
 await page.goto(url+'#working');
 await page.evaluate(()=>document.fonts.ready);
 let procs=await processSnapshot();
 const ids=new Set(procs.filter(p=>p.command.includes(profile)).map(p=>p.pid));
 let grew=true;while(grew){grew=false;for(const p of procs)if(ids.has(p.ppid)&&!ids.has(p.pid)){ids.add(p.pid);grew=true}}
 owned=procs.filter(p=>ids.has(p.pid)).map(({pid,start})=>({pid,start}));
 assert(owned.length>0,'Isolated browser process ownership identified');
 const version=context.browser().version();
 for(const [id,scene,file,width,height] of cases){
  await page.setViewportSize({width,height});
  await page.evaluate(s=>window.reference.setScene(s),scene);
  await page.evaluate(()=>document.fonts.ready);
  assert((await page.evaluate(()=>window.reference.getState())).scene===scene,`${id}: requested state rendered`);
  assert(await page.locator('[data-action="goal:exports"].selected').count()===1,`${id}: board selection matches the displayed goal`);
  assert(await page.locator('.detail-body').innerText().then(t=>t.includes('illustrative')),`${id}: explicit state detail rendered`);
  if(scene==='decision')await page.locator('[data-action="send-answer"]').scrollIntoViewIfNeeded();
  const geometry=await page.locator('#product').boundingBox();
  const overflow=await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth);
  assert(!overflow,`${id}: no horizontal document overflow at ${width}×${height}`);
  await page.screenshot({path:path.join(root,file),animations:'disabled'});
  captures.push({id,scene,file,width,height,deviceScaleFactor:1,productRegion:geometry,sha256:sha(await fs.readFile(path.join(root,file))),captureScope:'HTML design target; not native execution evidence'});
 }
 await page.setViewportSize({width:1440,height:900});
 await page.evaluate(()=>window.reference.setScene('start'));
 await page.locator('#goal-input').fill('');
 await page.locator('#goal-form button[type="submit"]').click();
 assert((await page.evaluate(()=>window.reference.getState())).scene==='start','Empty goal does not submit');
 await page.locator('#goal-input').fill('Implement specs/customer-exports/ end to end.');
 await page.locator('#goal-form button[type="submit"]').click();
 assert((await page.evaluate(()=>window.reference.getState())).scene==='working','Start navigates to the same goal experience in the prototype');
 await page.locator('#message-input').fill('Preserve the manual migration boundary.');
 await page.locator('.summary-links [data-action="detail:knowledge"]').click();
 await page.locator('[aria-label="Close goal details"]').click();
 assert(await page.locator('#message-input').inputValue()==='Preserve the manual migration boundary.','Draft survives details open and close');
 await page.locator('[data-action="goal:tests"]').click();
 assert(await page.locator('#message-input').inputValue()==='','Another goal does not inherit the first goal draft');
 await page.locator('[data-action="goal:exports"]').click();
 assert(await page.locator('#message-input').inputValue()==='Preserve the manual migration boundary.','Returning to a goal restores its scoped draft');
 await page.evaluate(()=>window.reference.setScene('decision'));
 assert(await page.locator('[data-action="send-answer"]').isDisabled(),'A displayed recommendation does not submit a decision');
 await page.locator('[data-action="answer:30"]').click();
 assert(await page.locator('[data-action="send-answer"]').isEnabled(),'Choosing an answer enables explicit submission');
 await page.locator('[data-action="send-answer"]').click();
 assert(await page.locator('.user-message').filter({hasText:'Keep generated exports for 30 days.'}).count()===1,'Submitted answer remains in the prototype conversation');
 await page.locator('.pane-title [data-action="pause"]').click();
 assert((await page.evaluate(()=>window.reference.getState())).runOverride==='Pausing','Pause first displays unsettled Pausing state');
 await page.waitForFunction(()=>window.reference.getState().runOverride==='Paused');
 await page.locator('[data-action="resume"]').click();
 assert((await page.evaluate(()=>window.reference.getState())).runOverride==='','Resume returns to working in the simulation');
 await page.evaluate(()=>window.reference.setScene('knowledge'));
 await page.locator('[data-action="knowledge-diff"]').click();
 assert(await page.locator('#dialog').isVisible(),'Knowledge revision opens an inspectable diff');
 await page.keyboard.press('Escape');
 assert(!await page.locator('#dialog').isVisible(),'Escape dismisses the diff dialog');
 await page.setViewportSize({width:960,height:720});
 await page.evaluate(()=>window.reference.setScene('decision'));
 await page.locator('[aria-label="Open goal details"]').click();
 assert(await page.locator('.overlay-backdrop').isVisible(),'Narrow details uses the task overlay');
 await page.keyboard.press('Escape');
 assert((await page.evaluate(()=>window.reference.getState())).detail===null,'Escape closes narrow details');
 assert(await page.locator('[aria-label="Open goal details"]').evaluate(el=>el===document.activeElement),'Closing narrow details restores focus');
 assert(errors.length===0,'No prototype JavaScript errors: '+JSON.stringify(errors));
 assert(requests.length===0,'No external HTTP requests');
 manifest={schema:1,amendment:'A11–A13 implementation state preparation',kind:'proposed-design-reference',nativeAcceptance:false,approval:'Assistant-prepared within the existing native direction; no new user approval inferred',renderedAt:new Date().toISOString(),renderer:version,platform:process.platform,fontContract:'Segoe UI, system-ui, sans-serif; actual font rasterization is platform-specific',source:{file:'step4-reference.html',sha256:sha(await fs.readFile(path.join(root,'step4-reference.html')))},comparison:'Use productRegion; exclude the gallery control bar. Compare native composition and state with documented renderer/font allowances.',captures,prototypeChecks:checks,errors,externalRequests:requests.length};
}finally{
 if(context)await context.close();
 let remaining=[];
 for(let i=0;i<20;i++){
  const current=await processSnapshot();remaining=current.filter(p=>owned.some(o=>o.pid===p.pid&&o.start===p.start));
  if(!remaining.length)break;
  await new Promise(resolve=>setTimeout(resolve,100));
 }
 if(remaining.length)throw new Error('Owned browser descendants remain; profile retained: '+JSON.stringify({profile,pids:remaining.map(p=>p.pid)}));
 await fs.rm(profile,{recursive:true,force:true});
 if(manifest){manifest.cleanup={ownedBrowserProcesses:owned.length,remaining:0,temporaryProfileRemoved:true};await fs.writeFile(path.join(root,'step4-manifest.json'),JSON.stringify(manifest,null,2)+'\n');console.log(JSON.stringify({captures:captures.length,prototypeChecks:checks.length,errors,externalRequests:requests.length,cleanup:manifest.cleanup},null,2));}
}
