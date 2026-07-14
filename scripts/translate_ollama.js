#!/usr/bin/env node
const fs = require('fs');
const path = require('path');

const LANG_DIR = path.join(__dirname, '../static/lang');
const MODEL = 'gemma3:4b';

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

async function translate(obj, targetLang) {
  const prompt = `Translate this JSON to ${targetLang}. Keep structure, translate values only. Keep "Orgos", URLs, {placeholders}. Return ONLY valid JSON:\n${JSON.stringify(obj)}`;
  
  const response = await fetch('http://127.0.0.1:11434/api/generate', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model: MODEL, prompt, stream: false })
  });
  
  const data = await response.json();
  const match = data.response.match(/\{[\s\S]*\}/);
  if (match) return JSON.parse(match[0]);
  throw new Error('No JSON in response');
}

async function translateSection(langCode, section, sectionData) {
  const langName = LANGUAGES[langCode];
  try {
    return await translate(sectionData, langName);
  } catch (e) {
    console.error(`  ${section} failed: ${e.message}`);
    return sectionData;
  }
}

async function translateFile(langCode) {
  const langName = LANGUAGES[langCode];
  if (!langName) return;
  
  const enFile = path.join(LANG_DIR, 'en.json');
  const targetFile = path.join(LANG_DIR, `${langCode}.json`);
  
  const en = JSON.parse(fs.readFileSync(enFile, 'utf8'));
  const translated = {};
  
  console.log(`\nTranslating ${langName} (${langCode})...`);
  
  // Translate section by section to avoid token limits
  for (const [key, value] of Object.entries(en)) {
    process.stdout.write(`  ${key}...`);
    translated[key] = await translateSection(langCode, key, value);
    console.log(' ✓');
  }
  
  fs.writeFileSync(targetFile, JSON.stringify(translated, null, 2) + '\n');
  console.log(`✓ ${langCode} complete`);
}

async function main() {
  const langs = process.argv.slice(2);
  const toTranslate = langs.length > 0 ? langs : Object.keys(LANGUAGES);
  
  for (const lang of toTranslate) {
    if (lang === 'en') continue;
    await translateFile(lang);
  }
}

main().catch(console.error);
