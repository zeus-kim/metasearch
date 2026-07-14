#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

// Get API key from keychain
const OPENAI_API_KEY = process.env.OPENAI_API_KEY || 
  execSync('security find-generic-password -a "openai" -s "OPENAI_API_KEY" -w 2>/dev/null').toString().trim();

const LANG_DIR = path.join(__dirname, '../static/lang');

const LANGUAGES = {
  af: 'Afrikaans', ar: 'Arabic', bg: 'Bulgarian', bn: 'Bengali', ca: 'Catalan',
  cs: 'Czech', da: 'Danish', de: 'German', el: 'Greek', es: 'Spanish',
  et: 'Estonian', eu: 'Basque', fa: 'Persian', fi: 'Finnish', fr: 'French',
  gl: 'Galician', he: 'Hebrew', hi: 'Hindi', hr: 'Croatian', hu: 'Hungarian',
  hy: 'Armenian', id: 'Indonesian', is: 'Icelandic', it: 'Italian', ja: 'Japanese',
  ka: 'Georgian', kk: 'Kazakh', km: 'Khmer', ko: 'Korean', lt: 'Lithuanian',
  lv: 'Latvian', mk: 'Macedonian', ml: 'Malayalam', mn: 'Mongolian', mr: 'Marathi',
  ms: 'Malay', my: 'Burmese', ne: 'Nepali', nl: 'Dutch', no: 'Norwegian',
  pa: 'Punjabi', pl: 'Polish', pt: 'Portuguese', ro: 'Romanian', ru: 'Russian',
  si: 'Sinhala', sk: 'Slovak', sl: 'Slovenian', sq: 'Albanian', sr: 'Serbian',
  sv: 'Swedish', sw: 'Swahili', ta: 'Tamil', te: 'Telugu', th: 'Thai',
  tl: 'Filipino', tr: 'Turkish', uk: 'Ukrainian', ur: 'Urdu', uz: 'Uzbek',
  vi: 'Vietnamese', zh: 'Chinese Simplified'
};

async function translate(text, targetLang) {
  const response = await fetch('https://api.openai.com/v1/chat/completions', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${OPENAI_API_KEY}`
    },
    body: JSON.stringify({
      model: 'gpt-4o-mini',
      messages: [{
        role: 'system',
        content: `Translate JSON values to ${targetLang}. Keep keys unchanged. Keep "Orgos", URLs, {placeholders} unchanged. Output ONLY valid JSON, no explanation.`
      }, {
        role: 'user',
        content: JSON.stringify(text)
      }],
      temperature: 0.2
    })
  });
  
  const data = await response.json();
  if (data.error) throw new Error(data.error.message);
  
  const content = data.choices[0].message.content;
  const match = content.match(/\{[\s\S]*\}/);
  if (match) return JSON.parse(match[0]);
  throw new Error('No JSON found');
}

async function translateFile(langCode) {
  const langName = LANGUAGES[langCode];
  if (!langName || langCode === 'en') return;
  
  const enFile = path.join(LANG_DIR, 'en.json');
  const targetFile = path.join(LANG_DIR, `${langCode}.json`);
  
  const en = JSON.parse(fs.readFileSync(enFile, 'utf8'));
  
  console.log(`Translating to ${langName} (${langCode})...`);
  
  try {
    const translated = await translate(en, langName);
    fs.writeFileSync(targetFile, JSON.stringify(translated, null, 2) + '\n');
    console.log(`✓ ${langCode} done`);
    return true;
  } catch (e) {
    console.error(`✗ ${langCode} failed: ${e.message}`);
    return false;
  }
}

async function main() {
  const langs = process.argv.slice(2);
  const toTranslate = langs.length > 0 ? langs : Object.keys(LANGUAGES);
  
  let success = 0, fail = 0;
  for (const lang of toTranslate) {
    if (lang === 'en') continue;
    const ok = await translateFile(lang);
    if (ok) success++; else fail++;
    await new Promise(r => setTimeout(r, 300));
  }
  console.log(`\nDone: ${success} success, ${fail} failed`);
}

main();
