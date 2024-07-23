// SPDX-FileCopyrightText: © 2024 David Bliss
//
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::photo::model::PictureId;

use crate::machine_learning::face_extractor;
use crate::path_encoding;
use crate::people::model;
use crate::people::FaceId;
use crate::people::PersonId;
use anyhow::*;
use rusqlite;
use rusqlite::params;
use rusqlite::Row;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Repository of people data.
/// Repository is backed by a Sqlite database.
#[derive(Debug, Clone)]
pub struct Repository {
    /// Base path to picture library on file system
    library_base_path: PathBuf,

    /// Base path for photo thumbnails and motion photo videos
    cache_dir_base_path: PathBuf,

    /// Connection to backing Sqlite database.
    con: Arc<Mutex<rusqlite::Connection>>,
}

impl Repository {
    /// Builds a Repository and creates operational tables.
    pub fn open(
        library_base_path: &Path,
        cache_dir_base_path: &Path,
        con: Arc<Mutex<rusqlite::Connection>>,
    ) -> Result<Repository> {
        if !library_base_path.is_dir() {
            bail!("{:?} is not a directory", library_base_path);
        }

        let library_base_path = PathBuf::from(library_base_path);
        let cache_dir_base_path = PathBuf::from(cache_dir_base_path);

        let repo = Repository {
            library_base_path,
            cache_dir_base_path,
            con,
        };

        Ok(repo)
    }

    /// FIXME should all the *face* functions move to a new repository?
    /// Gets all pictures that haven't been inspected for containing a motion photo.
    pub fn find_need_face_scan(&self) -> Result<Vec<(PictureId, PathBuf)>> {
        let con = self.con.lock().unwrap();
        let mut stmt = con.prepare(
            "SELECT
                    pictures.picture_id,
                    pictures.picture_path_b64,
                    COALESCE(
                        pictures.exif_created_ts,
                        pictures.exif_modified_ts,
                        pictures.fs_created_ts,
                        pictures.fs_modified_ts,
                        CURRENT_TIMESTAMP
                    ) AS ordering_ts
                FROM pictures
                LEFT OUTER JOIN pictures_face_scans USING (picture_id)
                WHERE pictures_face_scans.picture_id IS NULL
                AND COALESCE(pictures.is_broken, FALSE) IS FALSE
                ORDER BY ordering_ts DESC",
        )?;

        let result = stmt
            .query_map([], |row| self.to_picture_id_path_tuple(row))?
            .flatten()
            .collect();

        Ok(result)
    }

    pub fn find_faces(
        &self,
        picture_id: &PictureId,
    ) -> Result<Vec<(model::Face, Option<model::Person>)>> {
        let con = self.con.lock().unwrap();
        let mut stmt = con.prepare(
            "SELECT
                pictures_faces.face_id AS face_id,
                pictures_faces.thumbnail_path AS face_thumbnail_path,
                people.person_id AS person_id,
                people.name AS person_name,
                people.thumbnail_path AS person_thumbnail_path
            FROM pictures_faces
            LEFT OUTER JOIN people USING (person_id)
            WHERE picture_id = ?1 AND pictures_faces.is_face = TRUE",
        )?;

        let result = stmt
            .query_map([picture_id.id()], |row| self.to_face_and_person(row))?
            .flatten()
            .collect();

        Ok(result)
    }

    pub fn mark_face_scan_broken(&mut self, picture_id: &PictureId) -> Result<()> {
        let mut con = self.con.lock().unwrap();
        let tx = con.transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO pictures_face_scans (
                    picture_id,
                    is_broken,
                    face_count,
                    scan_ts
                ) VALUES (
                    ?1, TRUE, 0, CURRENT_TIMESTAMP
                ) ON CONFLICT (picture_id) DO UPDATE SET
                    is_broken = true,
                    face_count = 0,
                    scan_ts = CURRENT_TIMESTAMP
                ",
            )?;

            stmt.execute(params![picture_id.id(),])?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn add_face_scans(
        &mut self,
        picture_id: &PictureId,
        faces: &Vec<face_extractor::Face>,
    ) -> Result<()> {
        let mut con = self.con.lock().unwrap();
        let tx = con.transaction()?;

        // Create a scope to make borrowing of tx not be an error.
        {
            let mut scan_insert_stmt = tx.prepare_cached(
                "INSERT INTO pictures_face_scans (
                    picture_id,
                    is_broken,
                    face_count,
                    scan_ts
                ) VALUES (
                    ?1, ?2, ?3, CURRENT_TIMESTAMP
                ) ON CONFLICT (picture_id) DO UPDATE SET
                    is_broken = ?2,
                    face_count = ?3,
                    scan_ts = CURRENT_TIMESTAMP
                ",
            )?;

            scan_insert_stmt.execute(params![picture_id.id(), false, faces.len(),])?;

            let mut face_insert_stmt = tx.prepare_cached(
                "INSERT INTO pictures_faces (
                    picture_id,
                    thumbnail_path,
                    bounds_path,

                    model_name,

                    bounds_x,
                    bounds_y,
                    bounds_width,
                    bounds_height,

                    right_eye_x,
                    right_eye_y,

                    left_eye_x,
                    left_eye_y,

                    nose_x,
                    nose_y,

                    right_mouth_corner_x,
                    right_mouth_corner_y,

                    left_mouth_corner_x,
                    left_mouth_corner_y,

                    confidence,

                    is_face
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, true
                )
                ",
            )?;

            for face in faces {
                // convert to relative path before saving to database
                let thumbnail_path = face
                    .thumbnail_path
                    .strip_prefix(&self.cache_dir_base_path)?;
                let bounds_path = face.bounds_path.strip_prefix(&self.cache_dir_base_path)?;

                let right_eye = face.right_eye();
                let left_eye = face.left_eye();
                let nose = face.nose();
                let right_mouth_corner = face.right_mouth_corner();
                let left_mouth_corner = face.left_mouth_corner();

                face_insert_stmt.execute(params![
                    picture_id.id(),
                    thumbnail_path.to_string_lossy(),
                    bounds_path.to_string_lossy(),
                    face.model_name,
                    face.bounds.x,
                    face.bounds.y,
                    face.bounds.width,
                    face.bounds.height,
                    right_eye.map(|x| x.0),
                    right_eye.map(|x| x.1),
                    left_eye.map(|x| x.0),
                    left_eye.map(|x| x.1),
                    nose.map(|x| x.0),
                    nose.map(|x| x.1),
                    right_mouth_corner.map(|x| x.0),
                    right_mouth_corner.map(|x| x.1),
                    left_mouth_corner.map(|x| x.0),
                    left_mouth_corner.map(|x| x.1),
                    face.confidence
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    // FIXME probably need a mechanism to undo this in the likely event of user error.
    pub fn mark_not_a_face(&mut self, face_id: FaceId) -> Result<()> {
        let mut con = self.con.lock().unwrap();
        let tx = con.transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "UPDATE pictures_faces
                SET
                    is_face = FALSE
                WHERE face_id = ?1",
            )?;

            stmt.execute(params![face_id.id(),])?;
        }

        tx.commit()?;
        Ok(())
    }

    fn to_picture_id_path_tuple(&self, row: &Row<'_>) -> rusqlite::Result<(PictureId, PathBuf)> {
        let picture_id = row.get("picture_id").map(PictureId::new)?;

        let picture_path: String = row.get("picture_path_b64")?;
        let picture_path =
            path_encoding::from_base64(&picture_path).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let picture_path = self.library_base_path.join(picture_path);

        std::result::Result::Ok((picture_id, picture_path))
    }

    fn to_face_and_person(
        &self,
        row: &Row<'_>,
    ) -> rusqlite::Result<(model::Face, Option<model::Person>)> {
        let face_id = row.get("face_id").map(FaceId::new)?;

        let face_thumbnail_path = row
            .get("face_thumbnail_path")
            .map(|p: String| self.cache_dir_base_path.join(p))?;

        let face = model::Face {
            face_id,
            thumbnail_path: face_thumbnail_path,
        };

        let person_id = row.get("person_id").map(PersonId::new).ok();

        let person_name = row.get("person_name").ok();

        let person_thumbnail_path = row
            .get("person_thumbnail_path")
            .map(|p: String| self.cache_dir_base_path.join(p))
            .ok();

        let person = if let (Some(person_id), Some(name), Some(thumbnail_path)) =
            (person_id, person_name, person_thumbnail_path)
        {
            Some(model::Person {
                person_id,
                name,
                thumbnail_path,
            })
        } else {
            None
        };

        std::result::Result::Ok((face, person))
    }
}